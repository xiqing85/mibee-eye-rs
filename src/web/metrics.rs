use axum::http::StatusCode;
use axum::response::IntoResponse;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use std::sync::OnceLock;

/// Global handle to pull Prometheus formatted output.
static PROMETHEUS_HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

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
    async fn counters_render_after_emission() {
        init_metrics();
        // The wiring under test: call-site macros (same expressions the
        // product code uses) create and bump the series.
        ::metrics::counter!("mibee_rtsp_connections_total").increment(1);
        ::metrics::counter!("mibee_onvif_requests_total", "action" => "GetDeviceInformation")
            .increment(1);
        ::metrics::counter!("mibee_frames_captured_total").increment(1);
        ::metrics::counter!("mibee_motion_events_total").increment(1);
        ::metrics::counter!("mibee_storage_segments_total").increment(1);
        ::metrics::counter!("mibee_camera_errors_total").increment(1);
        ::metrics::gauge!("mibee_active_subscribers").set(2.0);

        let handle = PROMETHEUS_HANDLE.get().expect("init_metrics not called");
        let output = handle.render();
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
        assert!(
            output.contains(r#"mibee_onvif_requests_total{action="GetDeviceInformation"} 1"#),
            "labeled onvif series renders: {output}"
        );
    }
}
