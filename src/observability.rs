//! Internal call-chain tracing (SPEC v1 §3.3 + appendix A #37).
//!
//! The product's console/ring logging stays on the `log` facade
//! (`web::observe::TeeLogger` — the protocol libraries log there). This
//! module owns the **span** surface: when `[observability]
//! otlp_endpoint` is configured, a `tracing` subscriber with an OTLP
//! gRPC exporter is installed and the span macros sprinkled through the
//! hot paths (HTTP requests, RTSP sessions, GB28181 registration, AI
//! inference, recording segments) flow to external collectors
//! (Jaeger/Tempo/SigNoz). When the endpoint is empty — the default —
//! **no subscriber is installed** and every span macro compiles down to
//! a no-op: zero-cost when off, opt-in when a collector exists.
//!
//! Fail-open: an unreachable collector is a startup warning (the batch
//! exporter retries on its own schedule); tracing must never affect the
//! camera pipeline.

use anyhow::Result;
use opentelemetry::global;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace as sdktrace;
use opentelemetry_sdk::Resource;

/// Install the W3C propagator and, when `otlp_endpoint` is non-empty,
/// the OTLP-exporting tracing subscriber. Idempotent-ish: called once at
/// startup from `main`.
pub fn init_span_export(otlp_endpoint: &str) {
    // W3C TraceContext extraction/injection is harmless without a
    // subscriber and lets `http_request` spans adopt inbound
    // `traceparent` headers.
    global::set_text_map_propagator(TraceContextPropagator::new());

    if otlp_endpoint.is_empty() {
        return;
    }
    match install_otlp_subscriber(otlp_endpoint) {
        Ok(()) => log::info!(target: "mibee::observability",
                            "otlp span export enabled: {}", otlp_endpoint),
        Err(e) => log::warn!(target: "mibee::observability",
                            "otlp span export unavailable ({}), continuing without it (fail-open)", e),
    }
}

/// Build the OTLP (tonic/gRPC) tracer provider and install it as the
/// global tracing subscriber.
fn install_otlp_subscriber(endpoint: &str) -> Result<()> {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()?;

    let provider = sdktrace::SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(Resource::builder().with_service_name("mibee-eye").build())
        .build();

    let tracer = provider.tracer("mibee-eye");
    global::set_tracer_provider(provider);

    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
    tracing_subscriber::registry().with(otel_layer).init();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_endpoint_installs_no_subscriber() {
        // Must be a silent no-op (default path for existing deployments).
        init_span_export("");
    }

    #[test]
    fn unreachable_endpoint_fails_open() {
        // A bad endpoint logs a warning and returns — never panics. The
        // tonic exporter builder needs a Tokio runtime context.
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let _guard = rt.enter();
        init_span_export("http://127.0.0.1:19999");
    }
}
