use axum::http::StatusCode;
use axum::response::IntoResponse;
use metrics::{Counter, Gauge};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use std::sync::OnceLock;

/// Global handle to pull Prometheus formatted output.
static PROMETHEUS_HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// Application-level metric handles.
///
/// Each field corresponds to a Prometheus metric that is registered exactly
/// once when [`AppMetrics::register`] is called.
pub struct AppMetrics {
    rtsp_connections: Counter,
    onvif_requests: Counter,
    frames_captured: Counter,
    motion_events: Counter,
    storage_segments: Counter,
    camera_errors: Counter,
    active_subscribers: Gauge,
}

impl AppMetrics {
    /// Register all metrics with the global recorder and return the handles.
    ///
    /// Must be called once at application startup (panics if the recorder
    /// has not been initialised with [`init_metrics`]).
    #[must_use]
    pub fn register() -> Self {
        Self {
            rtsp_connections: metrics::counter!("mibee_rtsp_connections_total"),
            onvif_requests: metrics::counter!("mibee_onvif_requests_total"),
            frames_captured: metrics::counter!("mibee_frames_captured_total"),
            motion_events: metrics::counter!("mibee_motion_events_total"),
            storage_segments: metrics::counter!("mibee_storage_segments_total"),
            camera_errors: metrics::counter!("mibee_camera_errors_total"),
            active_subscribers: metrics::gauge!("mibee_active_subscribers"),
        }
    }

    /// Increment the RTSP connection counter.
    pub fn inc_rtsp_connection(&self) {
        self.rtsp_connections.increment(1);
    }

    /// Increment the ONVIF request counter.
    pub fn inc_onvif_request(&self) {
        self.onvif_requests.increment(1);
    }

    /// Increment the frames captured counter.
    pub fn inc_frame_captured(&self) {
        self.frames_captured.increment(1);
    }

    /// Increment the motion event counter.
    pub fn inc_motion_event(&self) {
        self.motion_events.increment(1);
    }

    /// Increment the storage segment counter.
    pub fn inc_storage_segment(&self) {
        self.storage_segments.increment(1);
    }

    /// Increment the camera error counter.
    pub fn inc_camera_error(&self) {
        self.camera_errors.increment(1);
    }

    /// Set the number of active subscribers (gauge).
    pub fn set_active_subscribers(&self, count: i64) {
        self.active_subscribers.set(count as f64);
    }
}

/// Initialise the Prometheus exporter recorder.
///
/// Must be called before [`AppMetrics::register`]. Safe to call more than
/// once — the second and subsequent calls are no-ops.
pub fn init_metrics() {
    PROMETHEUS_HANDLE.get_or_init(|| {
        PrometheusBuilder::new()
            .install_recorder()
            .expect("failed to install Prometheus recorder")
    });
}

/// Handler for `GET /metrics` — returns Prometheus text exposition format.
pub async fn metrics_handler() -> impl IntoResponse {
    let handle = PROMETHEUS_HANDLE
        .get()
        .expect("metrics handler called before init_metrics");
    let body = handle.render();
    (
        StatusCode::OK,
        [("content-type", "text/plain; charset=utf-8")],
        body,
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use tower::ServiceExt;

    /// Helper — build a router with the metrics endpoint only.
    fn test_router() -> axum::Router {
        init_metrics();
        let _metrics = AppMetrics::register();
        axum::Router::new().route("/metrics", axum::routing::get(metrics_handler))
    }

    #[tokio::test]
    async fn metrics_handler_returns_text() {
        let app = test_router();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .method(Method::GET)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            content_type.starts_with("text/plain"),
            "expected text/plain, got {content_type}"
        );
    }

    #[tokio::test]
    async fn metrics_contains_names() {
        let app = test_router();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .method(Method::GET)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let output = String::from_utf8_lossy(&body);

        // All expected metric names must appear.
        for name in &[
            "mibee_rtsp_connections_total",
            "mibee_onvif_requests_total",
            "mibee_frames_captured_total",
            "mibee_motion_events_total",
            "mibee_storage_segments_total",
            "mibee_camera_errors_total",
            "mibee_active_subscribers",
        ] {
            assert!(output.contains(name), "metric {name} not found in output");
        }
    }

    #[tokio::test]
    async fn counter_increments() {
        init_metrics();
        let _metrics = AppMetrics::register();

        let before = extract_counter_value("mibee_rtsp_connections_total");

        _metrics.inc_rtsp_connection();
        _metrics.inc_rtsp_connection();
        _metrics.inc_rtsp_connection();

        let after = extract_counter_value("mibee_rtsp_connections_total");

        assert_eq!(after - before, 3.0, "counter should have increased by 3");
    }

    /// Extract the current value of a counter from the Prometheus output.
    fn extract_counter_value(name: &str) -> f64 {
        let handle = PROMETHEUS_HANDLE.get().expect("init_metrics not called");
        let output = handle.render();
        for line in output.lines() {
            if line.starts_with(&format!("{name} ")) || line.starts_with(&format!("{name}\t")) {
                // Line format: "mibee_xxx_total 123"
                if let Some(val_str) = line.split_whitespace().nth(1) {
                    if let Ok(val) = val_str.parse::<f64>() {
                        return val;
                    }
                }
            }
        }
        // If the counter is zero it may not appear; treat as 0.
        0.0
    }
}
