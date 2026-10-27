//! Logs to stderr (stdout is the JSON-RPC wire) and, when `OTLP_ENDPOINT`
//! is set, exports spans to the OTel collector over OTLP/gRPC. Tool spans
//! carry latency, `search.unresponsive_engines`, `search.cache_hit` and, per
//! URL, `extract.stage` (`http`, `render`, `failed`).

use opentelemetry::trace::TracerProvider;
use opentelemetry_otlp::{SpanExporter, WithExportConfig};
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_sdk::Resource;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

pub const SERVICE_NAME: &str = "mcp-websearch-server";
const DEFAULT_LOG_FILTER: &str = "info";

/// Flushes pending spans when dropped.
pub struct Telemetry {
    /// Held for its `Drop`, which flushes pending spans.
    _exporter: Option<Exporter>,
}

/// The OTLP pipeline. Its gRPC connection lives on a runtime of its own: on
/// the server's runtime, flushing at shutdown would block a thread that has
/// to drive the export.
struct Exporter {
    provider: SdkTracerProvider,
    runtime: Option<tokio::runtime::Runtime>,
}

impl Drop for Exporter {
    fn drop(&mut self) {
        if let Err(error) = self.provider.shutdown() {
            // The subscriber may already be gone; stderr is the only channel left.
            eprintln!("flushing traces failed: {error}");
        }
        if let Some(runtime) = self.runtime.take() {
            // Dropping a runtime inside another one panics; this does not.
            runtime.shutdown_background();
        }
    }
}

fn exporter(endpoint: &str) -> anyhow::Result<Exporter> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("otel-export")
        .enable_all()
        .build()?;
    let built = {
        let _on_export_runtime = runtime.enter();
        SpanExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .build()
    };
    let span_exporter = match built {
        Ok(span_exporter) => span_exporter,
        Err(error) => {
            runtime.shutdown_background();
            return Err(anyhow::anyhow!(
                "building the OTLP exporter failed: {error}"
            ));
        }
    };
    let provider = SdkTracerProvider::builder()
        .with_resource(Resource::builder().with_service_name(SERVICE_NAME).build())
        .with_batch_exporter(span_exporter)
        .build();
    Ok(Exporter {
        provider,
        runtime: Some(runtime),
    })
}

/// Installs the global subscriber, and the OTLP exporter when an endpoint
/// is configured.
pub fn init(otlp_endpoint: Option<&str>) -> anyhow::Result<Telemetry> {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_FILTER));
    let fmt = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_ansi(false);
    let exporter = otlp_endpoint.map(exporter).transpose()?;
    let otel = exporter.as_ref().map(|exporter| {
        tracing_opentelemetry::layer().with_tracer(exporter.provider.tracer(SERVICE_NAME))
    });
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt)
        .with(otel)
        .try_init()?;
    Ok(Telemetry {
        _exporter: exporter,
    })
}
