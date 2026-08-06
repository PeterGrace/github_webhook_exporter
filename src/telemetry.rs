use std::{io, sync::Arc};

#[cfg(test)]
mod otlp_test;
mod queue;
pub(crate) mod trace;

use opentelemetry::{trace::TracerProvider as _, KeyValue};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{
    LogExporter as OtlpLogExporter, SpanExporter as OtlpSpanExporter, WithExportConfig,
    WithHttpConfig,
};
use opentelemetry_sdk::{logs::SdkLoggerProvider, trace::SdkTracerProvider, Resource};
use thiserror::Error;
use tracing::{Metadata, Subscriber};
use tracing_subscriber::{
    filter::filter_fn,
    fmt::MakeWriter,
    layer::{Layer, SubscriberExt},
    EnvFilter, Registry,
};

use crate::config::{ExporterSettings, TelemetryConfig};
use queue::AdmissionBoundary;

const INSTRUMENTATION_SCOPE: &str = "github_webhook_exporter";

/// Whether remote OTLP export is enabled for at least one signal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TelemetryState {
    /// Only independent structured stderr logging is active.
    Disabled,
    /// At least one bounded OTLP signal pipeline is active.
    Enabled,
}

/// Owns the optional OpenTelemetry providers and their observable queue boundaries.
#[derive(Debug)]
pub struct TelemetryRuntime {
    state: TelemetryState,
    tracer_provider: Option<SdkTracerProvider>,
    logger_provider: Option<SdkLoggerProvider>,
    trace_queue: Option<Arc<AdmissionBoundary>>,
    log_queue: Option<Arc<AdmissionBoundary>>,
}

impl TelemetryRuntime {
    /// Returns whether any remote OTLP signal pipeline is active.
    pub fn state(&self) -> TelemetryState {
        self.state
    }

    /// Returns the exact number of trace records dropped at the application queue boundary.
    pub fn dropped_trace_records(&self) -> u64 {
        self.trace_queue.as_ref().map_or(0, |queue| queue.dropped())
    }

    /// Returns the exact number of log records dropped at the application queue boundary.
    pub fn dropped_log_records(&self) -> u64 {
        self.log_queue.as_ref().map_or(0, |queue| queue.dropped())
    }

    /// Returns the exact number of failed trace export requests.
    pub fn failed_trace_exports(&self) -> u64 {
        self.trace_queue
            .as_ref()
            .map_or(0, |queue| queue.failed_exports())
    }

    /// Returns the exact number of failed log export requests.
    pub fn failed_log_exports(&self) -> u64 {
        self.log_queue
            .as_ref()
            .map_or(0, |queue| queue.failed_exports())
    }

    /// Returns the number of trace records currently admitted for export.
    pub fn pending_trace_records(&self) -> usize {
        self.trace_queue.as_ref().map_or(0, |queue| queue.pending())
    }

    /// Returns the number of log records currently admitted for export.
    pub fn pending_log_records(&self) -> usize {
        self.log_queue.as_ref().map_or(0, |queue| queue.pending())
    }

    /// Requests immediate export of records already accepted by both providers.
    ///
    /// This method blocks the calling thread while provider workers flush. Async callers should
    /// invoke it through [`tokio::task::spawn_blocking`].
    ///
    /// # Errors
    ///
    /// Returns [`TelemetryError::Flush`] when either provider cannot flush its accepted records.
    pub fn force_flush(&self) -> Result<(), TelemetryError> {
        let trace_result = self
            .tracer_provider
            .as_ref()
            .map(SdkTracerProvider::force_flush);
        let log_result = self
            .logger_provider
            .as_ref()
            .map(SdkLoggerProvider::force_flush);
        if trace_result.is_some_and(|result| result.is_err())
            || log_result.is_some_and(|result| result.is_err())
        {
            return Err(TelemetryError::Flush);
        }
        Ok(())
    }
}

/// A redacted failure while constructing, installing, or flushing telemetry.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum TelemetryError {
    /// The tracing filter directive is invalid.
    #[error("RUST_LOG contains an invalid tracing filter")]
    InvalidFilter,
    /// The OTLP trace exporter could not be constructed.
    #[error("failed to construct the OTLP trace exporter")]
    TraceExporter,
    /// The OTLP log exporter could not be constructed.
    #[error("failed to construct the OTLP log exporter")]
    LogExporter,
    /// Another global tracing subscriber has already been installed.
    #[error("the global tracing subscriber is already initialized")]
    AlreadyInitialized,
    /// At least one remote provider could not flush accepted telemetry.
    #[error("failed to flush remote telemetry")]
    Flush,
}

/// Installs structured stderr logging and any configured bounded OTLP pipelines.
///
/// The returned runtime must remain alive for as long as telemetry should be exported.
///
/// # Errors
///
/// Returns [`TelemetryError`] when the tracing filter or an exporter is invalid, or when another
/// global subscriber has already been installed. Errors contain no endpoint or header values.
pub fn init(rust_log: &str, config: &TelemetryConfig) -> Result<TelemetryRuntime, TelemetryError> {
    let (runtime, subscriber) = build_runtime(rust_log, config, io::stderr)?;
    tracing::subscriber::set_global_default(subscriber)
        .map_err(|_| TelemetryError::AlreadyInitialized)?;
    Ok(runtime)
}

/// Installs a minimal error-level subscriber when normal initialization fails.
///
/// If a subscriber is already installed, this function leaves it unchanged.
pub fn init_fallback() {
    drop(
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_env_filter(EnvFilter::new("error"))
            .with_writer(io::stderr)
            .try_init(),
    );
}

fn build_runtime<W>(
    rust_log: &str,
    config: &TelemetryConfig,
    writer: W,
) -> Result<(TelemetryRuntime, impl Subscriber + Send + Sync), TelemetryError>
where
    W: for<'writer> MakeWriter<'writer> + Send + Sync + 'static,
{
    let filter = EnvFilter::try_new(rust_log).map_err(|_| TelemetryError::InvalidFilter)?;
    let resource = telemetry_resource(config);
    let (tracer_provider, trace_queue) = config
        .trace_exporter
        .as_ref()
        .map(|settings| build_trace_provider(settings, config, &resource))
        .transpose()?
        .map_or((None, None), |(provider, queue)| {
            (Some(provider), Some(queue))
        });
    let (logger_provider, log_queue) = config
        .log_exporter
        .as_ref()
        .map(|settings| build_log_provider(settings, config, &resource))
        .transpose()?
        .map_or((None, None), |(provider, queue)| {
            (Some(provider), Some(queue))
        });

    let trace_layer = tracer_provider.as_ref().map(|provider| {
        tracing_opentelemetry::layer()
            .with_tracer(provider.tracer(INSTRUMENTATION_SCOPE))
            .with_location(false)
            .with_threads(false)
            .with_target(false)
            .with_tracked_inactivity(false)
            .with_filter(filter_fn(application_metadata))
    });
    let log_layer = logger_provider.as_ref().map(|provider| {
        OpenTelemetryTracingBridge::new(provider).with_filter(filter_fn(application_metadata))
    });
    let subscriber = Registry::default()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(writer),
        )
        .with(trace_layer)
        .with(log_layer);
    let state = if config.is_enabled() {
        TelemetryState::Enabled
    } else {
        TelemetryState::Disabled
    };

    Ok((
        TelemetryRuntime {
            state,
            tracer_provider,
            logger_provider,
            trace_queue,
            log_queue,
        },
        subscriber,
    ))
}

fn build_trace_provider(
    settings: &ExporterSettings,
    config: &TelemetryConfig,
    resource: &Resource,
) -> Result<(SdkTracerProvider, Arc<AdmissionBoundary>), TelemetryError> {
    let exporter = OtlpSpanExporter::builder()
        .with_http()
        .with_endpoint(settings.endpoint())
        .with_timeout(settings.timeout)
        .with_headers(settings.headers())
        .build()
        .map_err(|_| TelemetryError::TraceExporter)?;
    let (processor, queue) =
        queue::span_processor(exporter, config.queue_capacity(), config.batch_size());
    let provider = SdkTracerProvider::builder()
        .with_resource(resource.clone())
        .with_span_processor(processor)
        .build();
    Ok((provider, queue))
}

fn build_log_provider(
    settings: &ExporterSettings,
    config: &TelemetryConfig,
    resource: &Resource,
) -> Result<(SdkLoggerProvider, Arc<AdmissionBoundary>), TelemetryError> {
    let exporter = OtlpLogExporter::builder()
        .with_http()
        .with_endpoint(settings.endpoint())
        .with_timeout(settings.timeout)
        .with_headers(settings.headers())
        .build()
        .map_err(|_| TelemetryError::LogExporter)?;
    let (processor, queue) =
        queue::log_processor(exporter, config.queue_capacity(), config.batch_size());
    let provider = SdkLoggerProvider::builder()
        .with_resource(resource.clone())
        .with_log_processor(processor)
        .build();
    Ok((provider, queue))
}

fn telemetry_resource(config: &TelemetryConfig) -> Resource {
    let attributes = std::iter::once(KeyValue::new("service.version", env!("CARGO_PKG_VERSION")))
        .chain(
            config
                .resource_attributes
                .iter()
                .map(|(key, value)| KeyValue::new(key.clone(), value.clone())),
        );
    Resource::builder_empty()
        .with_service_name(config.service_name().to_owned())
        .with_attributes(attributes)
        .build()
}

fn application_metadata(metadata: &Metadata<'_>) -> bool {
    is_application_target(metadata.target())
}

fn is_application_target(target: &str) -> bool {
    target == INSTRUMENTATION_SCOPE
        || target
            .strip_prefix(INSTRUMENTATION_SCOPE)
            .is_some_and(|suffix| suffix.starts_with("::"))
}

#[cfg(test)]
fn build_subscriber<W>(
    rust_log: &str,
    writer: W,
) -> Result<impl Subscriber + Send + Sync, TelemetryError>
where
    W: for<'writer> MakeWriter<'writer> + Send + Sync + 'static,
{
    let filter = EnvFilter::try_new(rust_log).map_err(|_| TelemetryError::InvalidFilter)?;
    Ok(Registry::default().with(filter).with(
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(writer),
    ))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        ffi::OsString,
        io::{self, Write},
        sync::{Arc, Mutex},
    };

    use crate::config::TelemetryConfig;

    use super::{build_runtime, build_subscriber, is_application_target, TelemetryState};

    #[derive(Clone, Default)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl SharedWriter {
        fn contents(&self) -> String {
            let bytes = self.0.lock().expect("capture lock is available").clone();
            String::from_utf8(bytes).expect("tracing output is UTF-8")
        }
    }

    impl Write for SharedWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("capture lock is available")
                .extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for SharedWriter {
        type Writer = Self;

        fn make_writer(&'writer self) -> Self::Writer {
            self.clone()
        }
    }

    fn telemetry_config(values: &[(&str, &str)]) -> TelemetryConfig {
        let variables: HashMap<String, OsString> = values
            .iter()
            .map(|(name, value)| ((*name).to_owned(), OsString::from(value)))
            .collect();
        TelemetryConfig::from_lookup(&mut |variable| variables.get(variable).cloned())
            .expect("telemetry configuration is valid")
    }

    #[test]
    fn subscriber_honors_the_validated_filter() {
        let output = SharedWriter::default();
        let subscriber = build_subscriber("github_webhook_exporter=debug", output.clone())
            .expect("filter is valid");

        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!(target: "github_webhook_exporter", "visible-debug-event");
            tracing::debug!(target: "unrelated_dependency", "hidden-debug-event");
        });

        let rendered = output.contents();
        assert!(rendered.contains("visible-debug-event"));
        assert!(!rendered.contains("hidden-debug-event"));
    }

    #[test]
    fn disabled_runtime_preserves_structured_stderr() {
        let output = SharedWriter::default();
        let config = telemetry_config(&[]);
        let (runtime, subscriber) =
            build_runtime("github_webhook_exporter=info", &config, output.clone())
                .expect("disabled runtime builds");

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "github_webhook_exporter", "local-disabled-event");
        });

        assert_eq!(runtime.state(), TelemetryState::Disabled);
        assert!(output.contents().contains("local-disabled-event"));
    }

    #[test]
    fn enabled_runtime_preserves_structured_stderr() {
        let output = SharedWriter::default();
        let config = telemetry_config(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:9"),
            ("OTEL_EXPORTER_OTLP_TIMEOUT", "1"),
            ("GHE_OTEL_QUEUE_CAPACITY", "4"),
            ("GHE_OTEL_BATCH_SIZE", "1"),
        ]);
        let (runtime, subscriber) =
            build_runtime("github_webhook_exporter=info", &config, output.clone())
                .expect("enabled runtime builds");

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "github_webhook_exporter", "local-enabled-event");
        });

        assert_eq!(runtime.state(), TelemetryState::Enabled);
        assert!(output.contents().contains("local-enabled-event"));
    }

    #[test]
    fn remote_layers_accept_only_application_targets() {
        assert!(is_application_target("github_webhook_exporter"));
        assert!(is_application_target("github_webhook_exporter::api"));
        assert!(!is_application_target("opentelemetry_sdk"));
        assert!(!is_application_target("opentelemetry_otlp"));
        assert!(!is_application_target("unrelated_dependency"));
    }
}
