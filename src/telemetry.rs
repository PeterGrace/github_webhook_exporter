use std::{
    io,
    sync::{mpsc, Arc},
    thread,
    time::{Duration, Instant},
};

mod diagnostics;
mod http_client;
#[cfg(test)]
mod otlp_test;
mod queue;
pub(crate) mod trace;
pub(crate) mod workflow;

pub use workflow::WorkflowTraceEmitter;

use opentelemetry::{trace::TracerProvider as _, KeyValue};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{
    LogExporter as OtlpLogExporter, SpanExporter as OtlpSpanExporter, WithExportConfig,
    WithHttpConfig,
};
use opentelemetry_sdk::{
    logs::SdkLoggerProvider,
    trace::{SdkTracer, SdkTracerProvider},
    Resource,
};
use thiserror::Error;
use tracing::{Metadata, Subscriber};
use tracing_subscriber::{
    filter::filter_fn,
    fmt::MakeWriter,
    layer::{Layer, SubscriberExt},
    EnvFilter, Registry,
};

use crate::{
    config::{ExporterSettings, TelemetryConfig},
    metrics::Metrics,
};
use diagnostics::DiagnosticsObserver;
use http_client::ObservingHttpClient;
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
    workflow_trace_emitter: WorkflowTraceEmitter,
    trace_queue: Option<Arc<AdmissionBoundary>>,
    log_queue: Option<Arc<AdmissionBoundary>>,
    diagnostics: DiagnosticsObserver,
    shutdown_outcome: Option<TelemetryShutdownOutcome>,
}

/// The bounded result of shutting down all enabled remote telemetry providers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TelemetryShutdownOutcome {
    /// Every enabled provider shut down before the shared deadline.
    Completed,
    /// At least one provider returned a redacted shutdown failure.
    Failed,
    /// At least one provider remained unfinished at the shared deadline.
    TimedOut,
}

struct ShutdownTask {
    signal: crate::metrics::TelemetrySignal,
    operation: Box<dyn FnOnce(Duration) -> bool + Send + 'static>,
}

impl ShutdownTask {
    fn new(
        signal: crate::metrics::TelemetrySignal,
        operation: impl FnOnce(Duration) -> bool + Send + 'static,
    ) -> Self {
        Self {
            signal,
            operation: Box::new(operation),
        }
    }
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

    /// Returns the configured explicit-time historical workflow trace emitter.
    pub fn workflow_trace_emitter(&self) -> WorkflowTraceEmitter {
        self.workflow_trace_emitter.clone()
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

    /// Stops admission and shuts down both enabled providers within one shared deadline.
    ///
    /// The operation is idempotent: later calls return the first terminal outcome without
    /// invoking either provider again. Trace and log shutdown start concurrently, and `timeout`
    /// bounds their combined wait rather than granting a serial deadline to each signal.
    /// Provider failures are reduced to bounded diagnostics and never expose SDK error text.
    ///
    /// # Arguments
    ///
    /// * `timeout` - Maximum combined duration to wait for all enabled providers.
    ///
    /// # Returns
    ///
    /// A bounded outcome indicating completion, failure, or expiration of the shared deadline.
    pub fn shutdown(&mut self, timeout: Duration) -> TelemetryShutdownOutcome {
        if let Some(outcome) = self.shutdown_outcome {
            return outcome;
        }
        if let Some(queue) = &self.trace_queue {
            queue.close();
        }
        if let Some(queue) = &self.log_queue {
            queue.close();
        }

        let mut tasks = Vec::with_capacity(2);
        if let Some(provider) = self.tracer_provider.take() {
            tasks.push(ShutdownTask::new(
                crate::metrics::TelemetrySignal::Trace,
                move |remaining| provider.shutdown_with_timeout(remaining).is_ok(),
            ));
        }
        if let Some(provider) = self.logger_provider.take() {
            tasks.push(ShutdownTask::new(
                crate::metrics::TelemetrySignal::Log,
                move |remaining| provider.shutdown_with_timeout(remaining).is_ok(),
            ));
        }

        let outcome = run_shutdown_tasks(tasks, timeout, &self.diagnostics);
        if let Some(queue) = &self.trace_queue {
            queue.drop_pending();
        }
        if let Some(queue) = &self.log_queue {
            queue.drop_pending();
        }
        self.shutdown_outcome = Some(outcome);
        outcome
    }
}

fn run_shutdown_tasks(
    tasks: Vec<ShutdownTask>,
    timeout: Duration,
    diagnostics: &DiagnosticsObserver,
) -> TelemetryShutdownOutcome {
    use crate::metrics::{TelemetryExportFailureReason, TelemetrySignal};

    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now);
    let (sender, receiver) = mpsc::channel();
    let mut pending = [false; 2];
    let mut remaining_tasks = 0_usize;
    let mut failed = false;

    for task in tasks {
        let signal = task.signal;
        let slot = match signal {
            TelemetrySignal::Trace => 0,
            TelemetrySignal::Log => 1,
        };
        let sender = sender.clone();
        let remaining = deadline.saturating_duration_since(Instant::now());
        let spawn_result = thread::Builder::new()
            .name(format!("otel-{}-shutdown", signal.as_str()))
            .spawn(move || {
                let succeeded = (task.operation)(remaining);
                let _ignored = sender.send((signal, succeeded));
            });
        match spawn_result {
            Ok(_handle) => {
                pending[slot] = true;
                remaining_tasks += 1;
            }
            Err(_) => {
                failed = true;
                diagnostics.export_failure(signal, TelemetryExportFailureReason::Shutdown);
            }
        }
    }
    drop(sender);

    while remaining_tasks > 0 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match receiver.recv_timeout(remaining) {
            Ok((signal, succeeded)) => {
                let slot = match signal {
                    TelemetrySignal::Trace => 0,
                    TelemetrySignal::Log => 1,
                };
                if pending[slot] {
                    pending[slot] = false;
                    remaining_tasks -= 1;
                }
                if !succeeded {
                    failed = true;
                    diagnostics.export_failure(signal, TelemetryExportFailureReason::Shutdown);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    let mut timed_out = false;
    for (signal, unfinished) in [TelemetrySignal::Trace, TelemetrySignal::Log]
        .into_iter()
        .zip(pending)
    {
        if unfinished {
            timed_out = true;
            diagnostics.export_failure(signal, TelemetryExportFailureReason::Timeout);
        }
    }

    if timed_out {
        TelemetryShutdownOutcome::TimedOut
    } else if failed {
        TelemetryShutdownOutcome::Failed
    } else {
        TelemetryShutdownOutcome::Completed
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
pub fn init(
    rust_log: &str,
    config: &TelemetryConfig,
    metrics: Metrics,
) -> Result<TelemetryRuntime, TelemetryError> {
    let (runtime, subscriber) = build_runtime(rust_log, config, io::stderr, metrics)?;
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
    metrics: Metrics,
) -> Result<(TelemetryRuntime, impl Subscriber + Send + Sync), TelemetryError>
where
    W: for<'writer> MakeWriter<'writer> + Send + Sync + 'static,
{
    let filter = EnvFilter::try_new(rust_log).map_err(|_| TelemetryError::InvalidFilter)?;
    let resource = telemetry_resource(config);
    let observer = DiagnosticsObserver::new(metrics);
    let (tracer_provider, trace_tracer, workflow_trace_emitter, trace_queue) = config
        .trace_exporter
        .as_ref()
        .map(|settings| build_trace_provider(settings, config, &resource, observer.clone()))
        .transpose()?
        .map_or(
            (None, None, WorkflowTraceEmitter::disabled(), None),
            |(provider, tracer, queue)| {
                (
                    Some(provider),
                    Some(tracer.clone()),
                    WorkflowTraceEmitter::new(tracer),
                    Some(queue),
                )
            },
        );
    let (logger_provider, log_queue) = config
        .log_exporter
        .as_ref()
        .map(|settings| build_log_provider(settings, config, &resource, observer.clone()))
        .transpose()?
        .map_or((None, None), |(provider, queue)| {
            (Some(provider), Some(queue))
        });

    let trace_layer = trace_tracer.map(|tracer| {
        tracing_opentelemetry::layer()
            .with_tracer(tracer)
            .with_location(false)
            .with_threads(false)
            .with_target(false)
            .with_tracked_inactivity(false)
            .with_filter(filter_fn(application_trace_metadata))
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
            workflow_trace_emitter,
            trace_queue,
            log_queue,
            diagnostics: observer,
            shutdown_outcome: None,
        },
        subscriber,
    ))
}

fn build_trace_provider(
    settings: &ExporterSettings,
    config: &TelemetryConfig,
    resource: &Resource,
    observer: DiagnosticsObserver,
) -> Result<(SdkTracerProvider, SdkTracer, Arc<AdmissionBoundary>), TelemetryError> {
    let http_client =
        build_blocking_http_client(settings.timeout).map_err(|()| TelemetryError::TraceExporter)?;
    let http_client = ObservingHttpClient::new(
        http_client,
        crate::metrics::TelemetrySignal::Trace,
        observer.clone(),
    );
    let classified_failures = http_client.classified_failures();
    let exporter = OtlpSpanExporter::builder()
        .with_http()
        .with_http_client(http_client)
        .with_endpoint(settings.endpoint())
        .with_timeout(settings.timeout)
        .with_headers(settings.headers())
        .build()
        .map_err(|_| TelemetryError::TraceExporter)?;
    let (processor, queue) = queue::span_processor(
        exporter,
        config.queue_capacity(),
        config.batch_size(),
        observer,
        classified_failures,
    );
    let provider = SdkTracerProvider::builder()
        .with_resource(resource.clone())
        .with_span_processor(processor)
        .build();
    let tracer = provider.tracer(INSTRUMENTATION_SCOPE);
    Ok((provider, tracer, queue))
}

fn build_log_provider(
    settings: &ExporterSettings,
    config: &TelemetryConfig,
    resource: &Resource,
    observer: DiagnosticsObserver,
) -> Result<(SdkLoggerProvider, Arc<AdmissionBoundary>), TelemetryError> {
    let http_client =
        build_blocking_http_client(settings.timeout).map_err(|()| TelemetryError::LogExporter)?;
    let http_client = ObservingHttpClient::new(
        http_client,
        crate::metrics::TelemetrySignal::Log,
        observer.clone(),
    );
    let classified_failures = http_client.classified_failures();
    let exporter = OtlpLogExporter::builder()
        .with_http()
        .with_http_client(http_client)
        .with_endpoint(settings.endpoint())
        .with_timeout(settings.timeout)
        .with_headers(settings.headers())
        .build()
        .map_err(|_| TelemetryError::LogExporter)?;
    let (processor, queue) = queue::log_processor(
        exporter,
        config.queue_capacity(),
        config.batch_size(),
        observer,
        classified_failures,
    );
    let provider = SdkLoggerProvider::builder()
        .with_resource(resource.clone())
        .with_log_processor(processor)
        .build();
    Ok((provider, queue))
}

fn build_blocking_http_client(timeout: Duration) -> Result<reqwest::blocking::Client, ()> {
    thread::Builder::new()
        .name("otlp-http-client-init".to_owned())
        .spawn(move || {
            reqwest::blocking::Client::builder()
                .timeout(timeout)
                .build()
        })
        .map_err(|_| ())?
        .join()
        .map_err(|_| ())?
        .map_err(|_| ())
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

fn application_trace_metadata(metadata: &Metadata<'_>) -> bool {
    metadata.is_span() && application_metadata(metadata)
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
        net::TcpListener,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Barrier, Mutex,
        },
        thread,
        time::{Duration, Instant},
    };

    use crate::{
        config::TelemetryConfig,
        metrics::{Metrics, TelemetrySignal},
        telemetry::diagnostics::DiagnosticsObserver,
    };

    use super::{
        build_blocking_http_client, build_runtime, build_subscriber, is_application_target,
        run_shutdown_tasks, ShutdownTask, TelemetryShutdownOutcome, TelemetryState,
    };

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
    fn blocking_http_client_attempts_https_transport() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test listener binds");
        listener
            .set_nonblocking(true)
            .expect("test listener becomes nonblocking");
        let address = listener
            .local_addr()
            .expect("test listener address is available");
        let accepted = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(1);
            loop {
                match listener.accept() {
                    Ok((_stream, _peer)) => return true,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return false;
                        }
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("test listener failed: {error}"),
                }
            }
        });
        let client = build_blocking_http_client(Duration::from_secs(1))
            .expect("blocking HTTP client builds");

        let result = client.get(format!("https://{address}")).send();

        assert!(result.is_err(), "the fixture is not a TLS server");
        assert!(
            accepted.join().expect("test listener does not panic"),
            "the production client must open a connection for an HTTPS URL"
        );
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
        let (runtime, subscriber) = build_runtime(
            "github_webhook_exporter=info",
            &config,
            output.clone(),
            Metrics::new(),
        )
        .expect("disabled runtime builds");

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "github_webhook_exporter", "local-disabled-event");
        });

        assert_eq!(runtime.state(), TelemetryState::Disabled);
        assert!(output.contents().contains("local-disabled-event"));
    }

    #[test]
    fn enabled_runtime_preserves_structured_stderr() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test listener binds");
        let unavailable_endpoint = format!(
            "http://{}",
            listener
                .local_addr()
                .expect("test listener address is available")
        );
        drop(listener);
        let output = SharedWriter::default();
        let config = telemetry_config(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", &unavailable_endpoint),
            ("OTEL_EXPORTER_OTLP_TIMEOUT", "1000"),
            ("GHE_OTEL_QUEUE_CAPACITY", "4"),
            ("GHE_OTEL_BATCH_SIZE", "1"),
        ]);
        let metrics = Metrics::new();
        let (runtime, subscriber) = build_runtime(
            "github_webhook_exporter=info",
            &config,
            output.clone(),
            metrics.clone(),
        )
        .expect("enabled runtime builds");

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "github_webhook_exporter", "local-enabled-event");
        });

        assert_eq!(runtime.state(), TelemetryState::Enabled);
        assert!(output.contents().contains("local-enabled-event"));
        runtime.force_flush().expect("provider workers flush");
        assert!(metrics.encode().expect("metrics encode").contains(
            "github_telemetry_export_failures_total{signal=\"log\",reason=\"transport\"} 1"
        ));
    }

    #[test]
    fn remote_layers_accept_only_application_targets() {
        assert!(is_application_target("github_webhook_exporter"));
        assert!(is_application_target("github_webhook_exporter::api"));
        assert!(!is_application_target("opentelemetry_sdk"));
        assert!(!is_application_target("opentelemetry_otlp"));
        assert!(!is_application_target("unrelated_dependency"));
    }

    #[test]
    fn shutdown_starts_both_signals_before_waiting_for_either() {
        let barrier = Arc::new(Barrier::new(2));
        let trace_barrier = Arc::clone(&barrier);
        let log_barrier = Arc::clone(&barrier);
        let tasks = vec![
            ShutdownTask::new(TelemetrySignal::Trace, move |_| {
                trace_barrier.wait();
                true
            }),
            ShutdownTask::new(TelemetrySignal::Log, move |_| {
                log_barrier.wait();
                true
            }),
        ];

        let outcome = run_shutdown_tasks(
            tasks,
            Duration::from_secs(1),
            &DiagnosticsObserver::new(Metrics::new()),
        );

        assert_eq!(outcome, TelemetryShutdownOutcome::Completed);
    }

    #[test]
    fn shutdown_uses_one_deadline_when_one_signal_hangs() {
        let log_completed = Arc::new(AtomicBool::new(false));
        let completed = Arc::clone(&log_completed);
        let tasks = vec![
            ShutdownTask::new(TelemetrySignal::Trace, |_| {
                std::thread::sleep(Duration::from_secs(2));
                true
            }),
            ShutdownTask::new(TelemetrySignal::Log, move |_| {
                completed.store(true, Ordering::Release);
                true
            }),
        ];
        let metrics = Metrics::new();
        let started = Instant::now();

        let outcome = run_shutdown_tasks(
            tasks,
            Duration::from_millis(50),
            &DiagnosticsObserver::new(metrics.clone()),
        );

        assert_eq!(outcome, TelemetryShutdownOutcome::TimedOut);
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(log_completed.load(Ordering::Acquire));
        assert!(metrics.encode().expect("metrics encode").contains(
            "github_telemetry_export_failures_total{signal=\"trace\",reason=\"timeout\"} 1"
        ));
    }

    #[test]
    fn shutdown_failure_is_normalized_once() {
        let metrics = Metrics::new();
        let tasks = vec![ShutdownTask::new(TelemetrySignal::Log, |_| false)];

        let outcome = run_shutdown_tasks(
            tasks,
            Duration::from_secs(1),
            &DiagnosticsObserver::new(metrics.clone()),
        );

        assert_eq!(outcome, TelemetryShutdownOutcome::Failed);
        assert!(metrics.encode().expect("metrics encode").contains(
            "github_telemetry_export_failures_total{signal=\"log\",reason=\"shutdown\"} 1"
        ));
    }

    #[test]
    fn disabled_runtime_shutdown_is_idempotent() {
        let output = SharedWriter::default();
        let config = telemetry_config(&[]);
        let (mut runtime, _subscriber) = build_runtime(
            "github_webhook_exporter=info",
            &config,
            output,
            Metrics::new(),
        )
        .expect("disabled runtime builds");

        let first = runtime.shutdown(Duration::from_secs(1));
        let second = runtime.shutdown(Duration::from_secs(1));

        assert_eq!(first, TelemetryShutdownOutcome::Completed);
        assert_eq!(second, first);
    }
}
