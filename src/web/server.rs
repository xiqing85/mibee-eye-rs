use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::middleware::{from_fn_with_state, Next};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;

use crate::config::{Config, WebConfig};
use std::net::SocketAddr;
use std::time::Instant;
use tokio::net::TcpListener;

use super::embedded;
use super::error::WebError;
use super::metrics::{self, AppMetrics};
use super::video_stream::stream_mse_handler;
use std::sync::Arc;

use super::api;

/// Axum-based HTTP server for the web management interface.
pub struct WebServer {
    port: u16,
    enabled: bool,
    /// Shared YUV frame from camera for web snapshots.
    latest_yuv: Option<api::SharedYuvFrame>,
    /// H.264 AU hub for WebSocket video streaming.
    au_hub: Option<Arc<crate::h264::hub::AuHub>>,
    /// Latest AI detection results.
    last_detections: Option<Arc<tokio::sync::RwLock<Vec<crate::features::ai::Detection>>>>,
    /// Name of the ACTIVE AI detector (mock or ONNX), reported by
    /// /api/detections so the UI never mistakes a fallback for the real model.
    ai_model: Option<String>,
    /// The running AI module — hot model swap + authoritative active-model
    /// reporting (SPEC §4.6).
    ai_module: Option<Arc<crate::ai::AiModule>>,
    /// Detector factory for registry model ids (SPEC §4.6 activate).
    ai_loader: Option<crate::ai::registry::AiLoader>,
    /// Runtime model registry (builtin + uploaded overlay, SPEC §4.6).
    registry: Option<std::sync::Arc<std::sync::RwLock<crate::ai::registry::Registry>>>,
    /// Process-restart action for POST /api/system/restart (SPEC §5.1).
    restart_action: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Event bus for pipeline events.
    event_bus: Option<crate::pipeline::bus::EventBus>,
    /// Observability state (SPEC §3.2). Created eagerly so the process-wide
    /// logger can tee into it before the web server starts.
    observe: Arc<super::observe::Observe>,
}

impl WebServer {
    /// Create a new `WebServer` from the application web configuration.
    #[must_use]
    pub fn new(config: &WebConfig) -> Self {
        Self {
            port: config.port,
            enabled: config.enabled,
            latest_yuv: None,
            au_hub: None,
            last_detections: None,
            ai_model: None,
            ai_module: None,
            ai_loader: None,
            registry: None,
            restart_action: None,
            event_bus: None,
            observe: Arc::new(super::observe::Observe::new()),
        }
    }

    /// Share one [`Observe`] between the process logger (main) and the web
    /// state, so `/api/logs` sees entries emitted before startup too.
    #[must_use]
    pub fn with_observe(mut self, observe: Arc<super::observe::Observe>) -> Self {
        self.observe = observe;
        self
    }

    /// Set the shared YUV frame buffer for camera snapshots.
    pub fn with_camera_yuv(mut self, yuv: api::SharedYuvFrame) -> Self {
        self.latest_yuv = Some(yuv);
        self
    }

    pub fn with_au_hub(mut self, hub: Arc<crate::h264::hub::AuHub>) -> Self {
        self.au_hub = Some(hub);
        self
    }

    /// Set the latest AI detection results.
    pub fn with_latest_detections(
        mut self,
        detections: Arc<tokio::sync::RwLock<Vec<crate::features::ai::Detection>>>,
    ) -> Self {
        self.last_detections = Some(detections);
        self
    }

    /// Set the ACTIVE AI detector name (see [`crate::ai::AiModule::model_name`]).
    pub fn with_ai_model(mut self, model: String) -> Self {
        self.ai_model = Some(model);
        self
    }

    /// Set the running AI module for hot model switching (SPEC §4.6).
    pub fn with_ai_module(mut self, module: Arc<crate::ai::AiModule>) -> Self {
        self.ai_module = Some(module);
        self
    }

    /// Set the runtime model registry (SPEC §4.6). Absent leaves a
    /// builtin-only registry (uploads answer 501).
    pub fn with_registry(
        mut self,
        registry: std::sync::Arc<std::sync::RwLock<crate::ai::registry::Registry>>,
    ) -> Self {
        self.registry = Some(registry);
        self
    }

    /// Set the detector factory used by the activate endpoint (SPEC §4.6).
    /// Absent (plain builds without the `ai` feature) leaves capability
    /// `ai_models` false and activation answering 501.
    pub fn with_ai_loader(mut self, loader: crate::ai::registry::AiLoader) -> Self {
        self.ai_loader = Some(loader);
        self
    }

    /// Set the process-restart action for POST /api/system/restart (SPEC §5.1).
    pub fn with_restart_action(mut self, action: Arc<dyn Fn() + Send + Sync>) -> Self {
        self.restart_action = Some(action);
        self
    }

    /// Set the event bus for pipeline events.
    pub fn with_event_bus(mut self, bus: crate::pipeline::bus::EventBus) -> Self {
        self.event_bus = Some(bus);
        self
    }

    /// Build the axum [`Router`] with all route definitions (SPEC v1).
    ///
    /// Exposed as public so tests can exercise routing without binding.
    pub fn route(&self, state: Arc<api::AppState>) -> Router {
        metrics::init_metrics();
        let _app_metrics = AppMetrics::register();
        Router::new()
            // Public (SPEC §1–2)
            .route("/api/health", get(health_check))
            .route("/api/auth/me", get(super::auth::me_handler))
            .route("/api/auth/setup", post(super::auth::setup))
            .route("/api/auth/login", post(super::auth::login))
            .route("/api/auth/logout", post(super::auth::logout))
            .route("/api/auth/reset", post(super::auth::reset))
            // Session-gated core (SPEC §3–5)
            .route("/api/status", get(api::status_handler))
            .route("/api/capabilities", get(api::capabilities_handler))
            .route("/api/cameras", get(api::cameras_list))
            .route("/api/cameras/:id", get(api::camera_get))
            .route("/api/cameras/:id/snapshot", get(api::snapshot_handler))
            // Legacy public JPEG snapshot (SPEC appendix A dialect; the URI
            // ONVIF GetSnapshotUri advertises for NVR fetches).
            .route("/snapshot", get(api::legacy_snapshot_handler))
            .route("/api/cameras/:id/live", get(api::live_handler))
            .route("/api/cameras/:id/stream.mse", get(stream_mse_handler))
            .route(
                "/api/cameras/:id/recording",
                get(api::recording_status).post(api::recording_set),
            )
            .route("/api/config", get(api::get_config).put(api::put_config))
            .route("/api/reset", post(api::reset_config))
            .route("/api/events", get(super::events::events_handler))
            // Extensions (SPEC §4.6–4.7)
            .route("/api/ptz/status", get(api::ptz_status))
            .route("/api/ptz/move", post(api::ptz_move))
            .route("/api/detections", get(api::detections_handler))
            .route("/api/ai/models", get(api::ai_models_handler))
            .route("/api/ai/models/:id/activate", post(api::ai_model_activate))
            .route(
                "/api/ai/models/:id",
                post(api::ai_model_upload).delete(api::ai_model_delete),
            )
            // Model uploads exceed axum's 2 MB default body limit.
            .layer(axum::extract::DefaultBodyLimit::max(
                crate::ai::registry::UPLOAD_MAX_BYTES + (1 << 20),
            ))
            .route("/api/system/restart", post(api::system_restart))
            // Observability (SPEC §3.2)
            .route("/api/metrics/summary", get(super::observe::metrics_summary))
            .route("/api/logs", get(super::observe::logs_handler))
            .route("/api/requests", get(super::observe::requests_handler))
            // Device dialects (SPEC §1, appendix A5–A6)
            .route("/metrics", get(metrics::metrics_handler))
            .route("/onvif/*path", get(onvif_placeholder).post(onvif_soap_post))
            .fallback(embedded::handle_embedded)
            .layer(from_fn_with_state(state.clone(), super::auth::auth_gate))
            .layer(from_fn_with_state(state.clone(), request_logger))
            .with_state(state)
    }

    /// Start the HTTP server with the loaded application configuration.
    ///
    /// Returns immediately if the server is disabled.
    ///
    /// # Errors
    ///
    /// Returns [`WebError::Io`] if the listener cannot bind or the server
    /// encounters a fatal I/O error.
    pub async fn start(&self, config: Config, config_path: String) -> Result<(), WebError> {
        if !self.enabled {
            println!("web: server disabled, skipping");
            return Ok(());
        }
        // SPEC v1 §2 session auth: an empty web.password means first-boot —
        // the UI shows the setup flow until POST /api/auth/setup configures
        // the admin.
        if config.web.password.is_empty() {
            println!("web: admin not configured — first-time setup state");
        } else {
            let user = if config.web.username.is_empty() {
                "admin"
            } else {
                &config.web.username
            };
            println!("web: session authentication enabled (user: {user})");
        }

        // Sessions persist next to the config file (SPEC 附录A #10): the
        // §5.1 restart keeps browsers signed in instead of bouncing them
        // to the login page.
        let sessions_path = std::path::Path::new(&config_path).with_file_name("web-sessions.json");
        let recording_root = config.recording.storage_path.clone();
        let state = Arc::new(api::AppState {
            config: tokio::sync::RwLock::new(config),
            config_path,
            ptz: tokio::sync::RwLock::new(api::PtzStatus::default()),
            latest_yuv: self.latest_yuv.clone(),
            au_hub: self.au_hub.clone(),
            last_detections: self.last_detections.clone(),
            ai_model: self.ai_model.clone(),
            ai_module: self.ai_module.clone(),
            ai_loader: self.ai_loader.clone(),
            registry: self.registry.clone().unwrap_or_else(|| {
                std::sync::Arc::new(std::sync::RwLock::new(
                    crate::ai::registry::Registry::builtin_only(),
                ))
            }),
            restart_action: self.restart_action.clone(),
            event_bus: self.event_bus.clone(),
            sessions: std::sync::Arc::new(super::auth::SessionStore::with_persistence(
                sessions_path,
            )),
            started: std::time::Instant::now(),
            observe: self.observe.clone(),
        });
        super::auth::spawn_sweeper(state.sessions.clone());
        // Resource sampler for the metrics summary + Prometheus gauges.
        super::observe::spawn_sampler(
            state.observe.clone(),
            std::time::Duration::from_secs(2),
            recording_root,
        );
        let app = self.route(state);
        let addr = SocketAddr::from(([0, 0, 0, 0], self.port));

        let listener = TcpListener::bind(addr).await.map_err(WebError::Io)?;

        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await
            .map_err(WebError::Io)?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Route handlers
// ---------------------------------------------------------------------------

/// `GET /api/health` — public liveness probe (SPEC v1 §1).
async fn health_check(State(state): State<Arc<api::AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ok": true,
        "data": {"status": "ok", "uptime": state.started.elapsed().as_secs()}
    }))
}

/// Placeholder for ONVIF proxy routes (GET).
async fn onvif_placeholder() -> impl IntoResponse {
    Json(serde_json::json!({"error": "not implemented"}))
}

/// ONVIF SOAP POST handler — forwards the SOAP body to the dedicated ONVIF
/// server running on the ONVIF port and returns its SOAP response.
///
/// This allows NVRs to send SOAP requests through the web port.
async fn onvif_soap_post(State(state): State<Arc<api::AppState>>, body: String) -> Response {
    let onvif_port = state.config.read().await.onvif.port;

    let mut stream = match tokio::net::TcpStream::connect(format!("127.0.0.1:{onvif_port}")).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("onvif: proxy connect to 127.0.0.1:{onvif_port} failed: {e}");
            return soap_fault_response(&format!("ONVIF server unavailable: {e}"));
        }
    };

    let request = format!(
        "POST /onvif/device_service HTTP/1.1\r\n\
         Host: 127.0.0.1:{onvif_port}\r\n\
         Content-Type: application/soap+xml; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    );

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    if let Err(e) = stream.write_all(request.as_bytes()).await {
        eprintln!("onvif: proxy write failed: {e}");
        return soap_fault_response(&format!("ONVIF proxy write error: {e}"));
    }

    let mut response = Vec::new();
    if let Err(e) = stream.read_to_end(&mut response).await {
        eprintln!("onvif: proxy read failed: {e}");
        return soap_fault_response(&format!("ONVIF proxy read error: {e}"));
    }

    // Extract SOAP body from HTTP response (skip headers).
    let response_str = String::from_utf8_lossy(&response);
    let soap_body = response_str
        .split("\r\n\r\n")
        .nth(1)
        .unwrap_or(response_str.as_ref())
        .to_string();

    (
        StatusCode::OK,
        [("Content-Type", "application/soap+xml; charset=utf-8")],
        soap_body,
    )
        .into_response()
}

/// Build a SOAP 1.2 fault response.
fn soap_fault_response(reason: &str) -> Response {
    let fault = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope">
  <s:Body>
    <s:Fault>
      <s:Code><s:Value>s:Receiver</s:Value></s:Code>
      <s:Reason><s:Text xml:lang="en">{reason}</s:Text></s:Reason>
    </s:Fault>
  </s:Body>
</s:Envelope>"#
    );
    (
        StatusCode::OK,
        [("Content-Type", "application/soap+xml; charset=utf-8")],
        fault,
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Middleware
// ---------------------------------------------------------------------------

/// Simple request logging middleware (method, path, duration).
/// Request middleware (SPEC §3.2 tracing): assigns a request id (echoed as
/// `X-Request-Id`), records a trace entry for `/api/*` calls, bumps the
/// app-attributed HTTP traffic counters, and logs the outcome (which also
/// lands in the observability log ring via the tee logger).
async fn request_logger(
    axum::extract::State(state): axum::extract::State<Arc<api::AppState>>,
    request: Request<Body>,
    next: Next,
) -> impl IntoResponse {
    use axum::http::HeaderValue;
    use std::sync::atomic::Ordering;

    let method = request.method().clone();
    let uri = request.uri().path().to_owned();
    let start = Instant::now();
    let request_id = state.observe.alloc_request_id();
    let rx_bytes: u64 = request
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    state
        .observe
        .traffic
        .http_rx
        .fetch_add(rx_bytes, Ordering::Relaxed);

    let mut response = next.run(request).await;

    let status = response.status();
    let elapsed = start.elapsed();
    let tx_bytes: u64 = response
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    state
        .observe
        .traffic
        .http_tx
        .fetch_add(tx_bytes, Ordering::Relaxed);

    if uri.starts_with("/api/") {
        state
            .observe
            .push_request(super::observe::make_request_entry(
                request_id.clone(),
                method.as_str(),
                &uri,
                status.as_u16(),
                elapsed,
            ));
        ::metrics::counter!("mibee_http_requests_total", "status" => status.as_u16().to_string())
            .increment(1);
        log::info!(
            target: "mibee::http",
            "{} {} -> {} ({:?}) [req {}]",
            method, uri, status, elapsed, request_id
        );
    }

    if let Ok(v) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert("x-request-id", v);
    }
    response
}

// ---------------------------------------------------------------------------
// Graceful shutdown
// ---------------------------------------------------------------------------

/// Wait for SIGINT (Ctrl+C) to trigger a graceful shutdown.
async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install Ctrl+C handler");
    println!("web: shutdown signal received, stopping gracefully");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use std::sync::Arc;
    use tower::ServiceExt;

    fn default_state() -> Arc<api::AppState> {
        Arc::new(api::AppState::default())
    }

    #[tokio::test]
    async fn test_health_check_returns_200() {
        let server = WebServer::new(&WebConfig::default());
        let app = server.route(default_state());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .method(Method::GET)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_health_check_body() {
        let server = WebServer::new(&WebConfig::default());
        let app = server.route(default_state());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .method(Method::GET)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["ok"], "true".parse::<serde_json::Value>().unwrap());
        assert_eq!(json["data"]["status"], "ok");
        assert!(json["data"]["uptime"].is_u64(), "SPEC §1 uptime field");
    }

    /// Uptime must be anchored at state construction (process start), not at
    /// the first request — a device nobody polled for hours must still report
    /// its real uptime. Run standalone (`--exact`) to prove the red state:
    /// with a lazily-initialized clock the first request reports ~0.
    #[tokio::test]
    async fn test_health_uptime_anchored_at_construction() {
        let server = WebServer::new(&WebConfig::default());
        let app = server.route(default_state());
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .method(Method::GET)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let uptime = json["data"]["uptime"].as_u64().expect("uptime u64");
        assert!(
            uptime >= 1,
            "uptime must count from construction, got {uptime}"
        );
    }

    #[tokio::test]
    async fn test_auth_gate_blocks_gated_reads_without_session() {
        let server = WebServer::new(&WebConfig::default());
        let app = server.route(default_state());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/cameras")
                    .method(Method::GET)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    // -- Observability (SPEC §3.2) ---------------------------------------

    /// The metrics summary requires a session like every other /api read.
    #[tokio::test]
    async fn test_metrics_summary_requires_session() {
        let server = WebServer::new(&WebConfig::default());
        let app = server.route(default_state());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/metrics/summary")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    /// Configure admin credentials on a fresh state, log in through the real
    /// endpoint, and return (cookie header, app with same state).
    async fn login_admin(app: &Router, state: Arc<api::AppState>) -> String {
        state.config.write().await.web.password = "pw-test-12345".into();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/auth/login")
                    .method(Method::POST)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "username": "admin", "password": "pw-test-12345"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "login must succeed");
        response
            .headers()
            .get("set-cookie")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.split(';').next())
            .expect("session cookie")
            .to_string()
    }

    /// `/api/metrics/summary` shape: nested system/process objects with the
    /// SPEC §3.2 fields present.
    #[tokio::test]
    async fn test_metrics_summary_shape() {
        let state = default_state();
        // Seed a snapshot without waiting for the sampler.
        state
            .observe
            .record_sample(crate::web::observe::read_sample(), 4.0, "/mnt/data");
        let server = WebServer::new(&WebConfig::default());
        let app = server.route(state.clone());
        let cookie = login_admin(&app, state.clone()).await;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/metrics/summary")
                    .header("cookie", &cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let data = &json["data"];
        assert!(data["ts"].is_u64());
        assert!(data["system"]["cpu_percent"].is_number());
        assert!(data["system"]["memory"]["total"].is_u64());
        assert!(data["system"]["network"]["rx_rate"].is_number());
        assert!(data["system"]["disks"]
            .as_array()
            .expect("disks list")
            .iter()
            .any(|d| d["path"] == "/"));
        assert!(data["process"]["rss_bytes"].is_u64());
        assert!(data["process"]["cpu_percent"].is_number());
        assert!(data["process"]["storage_bytes"].is_u64());
        assert!(data["process"]["traffic"]["http_tx_bytes"].is_u64());
        assert!(data["process"]["uptime"].is_u64());
    }

    /// Every API response carries `X-Request-Id` and /api calls land in the
    /// trace ring with method/path/status/duration.
    #[tokio::test]
    async fn test_request_tracing_middleware() {
        let state = default_state();
        let server = WebServer::new(&WebConfig::default());
        let app = server.route(state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .header("content-length", "7")
                    .body(Body::from("payload"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(response.headers().get("x-request-id").is_some());
        let entries = state.observe.requests_newest_first();
        assert!(
            entries
                .iter()
                .any(|e| e.path == "/api/health" && e.status == 200),
            "health call must be traced, got {:?}",
            entries
        );
        // App-attributed HTTP receive bytes come from the request's
        // Content-Length. (Send-side counting needs an explicit response
        // Content-Length; streamed replies legitimately have none.)
        assert!(
            state
                .observe
                .traffic
                .http_rx
                .load(std::sync::atomic::Ordering::Relaxed)
                > 0
        );
    }

    /// `/api/logs` returns the ring (newest first) and `/api/requests`
    /// answers; both behind the session gate exercised above.
    #[tokio::test]
    async fn test_logs_and_requests_endpoints() {
        let state = default_state();
        state.observe.push_log(crate::web::observe::LogEntry {
            ts: 1,
            level: "info".into(),
            target: "test".into(),
            message: "ring-entry".into(),
            request_id: None,
        });
        let server = WebServer::new(&WebConfig::default());
        let app = server.route(state.clone());
        let cookie = login_admin(&app, state.clone()).await;
        for uri in ["/api/logs", "/api/requests"] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(uri)
                        .header("cookie", &cookie)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert!(json["data"]["entries"].is_array(), "{uri}: {json}");
        }
        let logs = state.observe.logs_newest_first();
        assert!(logs.iter().any(|e| e.message == "ring-entry"));
    }

    #[tokio::test]
    async fn test_root_serves_index() {
        let server = WebServer::new(&WebConfig::default());
        let app = server.route(default_state());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .method(Method::GET)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_unknown_api_is_unauthorized_not_spa() {
        let server = WebServer::new(&WebConfig::default());
        let app = server.route(default_state());
        // Unknown /api/* paths hit the auth gate before the SPA fallback —
        // they must answer 401, never leak the SPA shell.
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/unknown")
                    .method(Method::GET)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_not_found_falls_to_spa() {
        let server = WebServer::new(&WebConfig::default());
        let app = server.route(default_state());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/nonexistent-route")
                    .method(Method::GET)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // SPA fallback should serve index.html for unknown routes.
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_onvif_post_returns_soap_not_405() {
        // POST to /onvif/device_service must NOT return 405.
        // Since no ONVIF server is running in the test, it returns a SOAP fault,
        // which proves POST is accepted and processed.
        let server = WebServer::new(&WebConfig::default());
        let app = server.route(default_state());
        let soap = r#"<?xml version="1.0"?><soap:Envelope/>"#;
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/onvif/device_service")
                    .method(Method::POST)
                    .header("Content-Type", "application/soap+xml")
                    .body(Body::from(soap))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("Fault") || text.contains("soap"));
    }

    #[tokio::test]
    async fn test_onvif_get_placeholder_answers_json_error() {
        let server = WebServer::new(&WebConfig::default());
        let app = server.route(default_state());

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/onvif/device_service")
                    .method(Method::GET)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "not implemented");
    }

    #[tokio::test]
    async fn test_onvif_proxy_forwards_to_real_backend() {
        // Fake ONVIF backend: reads one full request, replies, closes.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let onvif_port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut sock, _) = listener.accept().await.unwrap();
            // Read headers + Content-Length body before answering (a real
            // ONVIF server does not wait for EOF — neither may the fake).
            let mut buf = Vec::new();
            let mut tmp = [0u8; 1024];
            loop {
                let n = sock.read(&mut tmp).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&buf[..pos]).to_string();
                    let clen = headers
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.trim()
                                .eq_ignore_ascii_case("Content-Length")
                                .then(|| v.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    if buf.len() >= pos + 4 + clen {
                        break;
                    }
                }
            }
            let soap_reply = b"<SOAP-REPLY-MARKER/>";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/soap+xml\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n",
                soap_reply.len()
            );
            sock.write_all(response.as_bytes()).await.unwrap();
            sock.write_all(soap_reply).await.unwrap();
            // Drop closes the socket so the proxy's read_to_end finishes.
        });

        let state = default_state();
        state.config.write().await.onvif.port = onvif_port;
        let server = WebServer::new(&WebConfig::default());
        let app = server.route(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/onvif/device_service")
                    .method(Method::POST)
                    .header("Content-Type", "application/soap+xml")
                    .body(Body::from("<GetDeviceInformation/>"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(
            text.contains("SOAP-REPLY-MARKER"),
            "proxy must relay the backend SOAP body, got: {text}"
        );
        assert!(!text.contains("HTTP/1.1"), "HTTP headers must be stripped");
    }
}
