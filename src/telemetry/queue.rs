use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::{future::Future, sync::Arc, time::Duration};

use opentelemetry::{logs::Severity, Context, InstrumentationScope};
use opentelemetry_sdk::{
    error::{OTelSdkError, OTelSdkResult},
    logs::{
        BatchConfigBuilder as LogBatchConfigBuilder, BatchLogProcessor, LogBatch, LogExporter,
        LogProcessor, SdkLogRecord,
    },
    trace::{
        BatchConfigBuilder as TraceBatchConfigBuilder, BatchSpanProcessor, Span, SpanData,
        SpanExporter, SpanProcessor,
    },
    Resource,
};

use crate::metrics::{TelemetryDropReason, TelemetryExportFailureReason, TelemetrySignal};
use crate::telemetry::diagnostics::DiagnosticsObserver;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AdmissionOutcome {
    Admitted,
    QueueFull,
    PipelineClosed,
}

#[derive(Debug)]
pub(super) struct AdmissionBoundary {
    capacity: usize,
    pending: AtomicUsize,
    dropped: AtomicU64,
    failed_exports: AtomicU64,
    closed: AtomicBool,
    signal: TelemetrySignal,
    observer: DiagnosticsObserver,
}

impl AdmissionBoundary {
    pub(super) fn new(
        capacity: usize,
        signal: TelemetrySignal,
        observer: DiagnosticsObserver,
    ) -> Self {
        debug_assert!(capacity > 0);
        Self {
            capacity,
            pending: AtomicUsize::new(0),
            dropped: AtomicU64::new(0),
            failed_exports: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            signal,
            observer,
        }
    }

    pub(super) fn try_admit(&self) -> AdmissionOutcome {
        if self.closed.load(Ordering::Acquire) {
            self.record_drop(TelemetryDropReason::PipelineClosed);
            return AdmissionOutcome::PipelineClosed;
        }
        let mut pending = self.pending.load(Ordering::Acquire);
        loop {
            if pending >= self.capacity {
                self.record_drop(TelemetryDropReason::QueueFull);
                return AdmissionOutcome::QueueFull;
            }
            match self.pending.compare_exchange_weak(
                pending,
                pending + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    if self.closed.load(Ordering::Acquire) {
                        self.release(1);
                        self.record_drop(TelemetryDropReason::PipelineClosed);
                        return AdmissionOutcome::PipelineClosed;
                    }
                    return AdmissionOutcome::Admitted;
                }
                Err(observed) => pending = observed,
            }
        }
    }

    fn record_drop(&self, reason: TelemetryDropReason) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
        self.observer.drop_record(self.signal, reason);
    }

    pub(super) fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    pub(super) fn release(&self, count: usize) {
        let mut pending = self.pending.load(Ordering::Acquire);
        loop {
            let remaining = pending.saturating_sub(count);
            match self.pending.compare_exchange_weak(
                pending,
                remaining,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => pending = observed,
            }
        }
    }

    pub(super) fn pending(&self) -> usize {
        self.pending.load(Ordering::Acquire)
    }

    pub(super) fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    pub(super) fn failed_exports(&self) -> u64 {
        self.failed_exports.load(Ordering::Relaxed)
    }
}

async fn observe_export(
    boundary: &AdmissionBoundary,
    classified_failures: &AtomicU64,
    export: impl Future<Output = OTelSdkResult>,
) -> OTelSdkResult {
    let classified_before = classified_failures.load(Ordering::Relaxed);
    let result = export.await;
    if let Err(error) = &result {
        boundary.failed_exports.fetch_add(1, Ordering::Relaxed);
        if classified_failures.load(Ordering::Relaxed) == classified_before {
            record_sdk_failure(boundary, error);
        }
    }
    result
}

fn record_sdk_failure(boundary: &AdmissionBoundary, error: &OTelSdkError) {
    let reason = match error {
        OTelSdkError::AlreadyShutdown => TelemetryExportFailureReason::Shutdown,
        OTelSdkError::Timeout(_) => TelemetryExportFailureReason::Timeout,
        OTelSdkError::InternalFailure(_) => TelemetryExportFailureReason::Internal,
    };
    boundary.observer.export_failure(boundary.signal, reason);
}

#[derive(Debug)]
struct BoundarySpanExporter<E> {
    exporter: E,
    boundary: Arc<AdmissionBoundary>,
    classified_failures: Arc<AtomicU64>,
}

impl<E: SpanExporter> SpanExporter for BoundarySpanExporter<E> {
    fn export(
        &self,
        batch: Vec<SpanData>,
    ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
        self.boundary.release(batch.len());
        observe_export(
            &self.boundary,
            &self.classified_failures,
            self.exporter.export(batch),
        )
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        let result = self.exporter.shutdown_with_timeout(timeout);
        if let Err(error) = &result {
            record_sdk_failure(&self.boundary, error);
        }
        result
    }

    fn force_flush(&self) -> OTelSdkResult {
        let result = self.exporter.force_flush();
        if let Err(error) = &result {
            record_sdk_failure(&self.boundary, error);
        }
        result
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.exporter.set_resource(resource);
    }
}

#[derive(Debug)]
pub(super) struct BoundarySpanProcessor {
    processor: BatchSpanProcessor,
    boundary: Arc<AdmissionBoundary>,
}

impl SpanProcessor for BoundarySpanProcessor {
    fn on_start(&self, span: &mut Span, context: &Context) {
        self.processor.on_start(span, context);
    }

    fn on_end(&self, span: SpanData) {
        if self.boundary.try_admit() == AdmissionOutcome::Admitted {
            self.processor.on_end(span);
        }
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.processor.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.boundary.close();
        self.processor.shutdown_with_timeout(timeout)
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.processor.set_resource(resource);
    }
}

/// Builds the trace processor with the SDK queue bounded by the same admission limit.
///
/// Admission increments `pending` before the SDK's non-blocking send, and `pending` is released
/// only after the SDK receiver has removed a batch. Consequently SDK queue occupancy is never
/// greater than `pending`: when both limits are `capacity`, this boundary rejects first and the SDK
/// queue cannot overflow. Provider shutdown must stop producers before disconnecting the SDK queue;
/// that lifecycle ordering is implemented by the dedicated Phase 4 shutdown work.
pub(super) fn span_processor<E: SpanExporter + 'static>(
    exporter: E,
    capacity: usize,
    batch_size: usize,
    observer: DiagnosticsObserver,
    classified_failures: Arc<AtomicU64>,
) -> (BoundarySpanProcessor, Arc<AdmissionBoundary>) {
    let boundary = Arc::new(AdmissionBoundary::new(
        capacity,
        TelemetrySignal::Trace,
        observer,
    ));
    let exporter = BoundarySpanExporter {
        exporter,
        boundary: Arc::clone(&boundary),
        classified_failures,
    };
    let batch_config = TraceBatchConfigBuilder::default()
        .with_max_queue_size(capacity)
        .with_max_export_batch_size(batch_size)
        .build();
    let processor = BatchSpanProcessor::builder(exporter)
        .with_batch_config(batch_config)
        .build();
    (
        BoundarySpanProcessor {
            processor,
            boundary: Arc::clone(&boundary),
        },
        boundary,
    )
}

#[derive(Debug)]
struct BoundaryLogExporter<E> {
    exporter: E,
    boundary: Arc<AdmissionBoundary>,
    classified_failures: Arc<AtomicU64>,
}

impl<E: LogExporter> LogExporter for BoundaryLogExporter<E> {
    fn export(
        &self,
        batch: LogBatch<'_>,
    ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
        // OpenTelemetry 0.32 exposes no public O(1) `LogBatch::len`; its iterator is exact.
        self.boundary.release(batch.iter().count());
        observe_export(
            &self.boundary,
            &self.classified_failures,
            self.exporter.export(batch),
        )
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        let result = self.exporter.shutdown_with_timeout(timeout);
        if let Err(error) = &result {
            record_sdk_failure(&self.boundary, error);
        }
        result
    }

    fn event_enabled(&self, level: Severity, target: &str, name: Option<&str>) -> bool {
        self.exporter.event_enabled(level, target, name)
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.exporter.set_resource(resource);
    }
}

#[derive(Debug)]
pub(super) struct BoundaryLogProcessor {
    processor: BatchLogProcessor,
    boundary: Arc<AdmissionBoundary>,
}

impl LogProcessor for BoundaryLogProcessor {
    fn emit(&self, record: &mut SdkLogRecord, scope: &InstrumentationScope) {
        if self.boundary.try_admit() == AdmissionOutcome::Admitted {
            self.processor.emit(record, scope);
        }
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.processor.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.boundary.close();
        self.processor.shutdown_with_timeout(timeout)
    }

    fn event_enabled(&self, level: Severity, target: &str, name: Option<&str>) -> bool {
        self.processor.event_enabled(level, target, name)
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.processor.set_resource(resource);
    }
}

/// Builds the log processor under the same occupancy invariant as [`span_processor`].
pub(super) fn log_processor<E: LogExporter + 'static>(
    exporter: E,
    capacity: usize,
    batch_size: usize,
    observer: DiagnosticsObserver,
    classified_failures: Arc<AtomicU64>,
) -> (BoundaryLogProcessor, Arc<AdmissionBoundary>) {
    let boundary = Arc::new(AdmissionBoundary::new(
        capacity,
        TelemetrySignal::Log,
        observer,
    ));
    let exporter = BoundaryLogExporter {
        exporter,
        boundary: Arc::clone(&boundary),
        classified_failures,
    };
    let batch_config = LogBatchConfigBuilder::default()
        .with_max_queue_size(capacity)
        .with_max_export_batch_size(batch_size)
        .build();
    let processor = BatchLogProcessor::builder(exporter)
        .with_batch_config(batch_config)
        .build();
    (
        BoundaryLogProcessor {
            processor,
            boundary: Arc::clone(&boundary),
        },
        boundary,
    )
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc, Barrier,
        },
        thread,
        time::Duration,
    };

    use opentelemetry_sdk::error::OTelSdkError;

    use crate::{
        metrics::{Metrics, TelemetryExportFailureReason, TelemetrySignal},
        telemetry::diagnostics::DiagnosticsObserver,
    };

    use super::{observe_export, AdmissionBoundary, AdmissionOutcome};

    fn boundary_with_metrics(capacity: usize) -> (AdmissionBoundary, Metrics) {
        let metrics = Metrics::new();
        let boundary = AdmissionBoundary::new(
            capacity,
            TelemetrySignal::Trace,
            DiagnosticsObserver::new(metrics.clone()),
        );
        (boundary, metrics)
    }

    fn boundary(capacity: usize) -> AdmissionBoundary {
        boundary_with_metrics(capacity).0
    }

    #[test]
    fn closed_admission_is_counted_as_pipeline_closed() {
        let metrics = Metrics::new();
        let observer = DiagnosticsObserver::new(metrics.clone());
        let boundary = AdmissionBoundary::new(2, TelemetrySignal::Trace, observer);

        boundary.close();

        assert_eq!(boundary.try_admit(), AdmissionOutcome::PipelineClosed);
        assert_eq!(boundary.dropped(), 1);
        assert!(metrics.encode().expect("metrics encode").contains(
            "github_telemetry_dropped_records_total{signal=\"trace\",reason=\"pipeline_closed\"} 1"
        ));
    }

    #[test]
    fn admission_capacity_is_exact_and_released_batches_restore_space() {
        let boundary = boundary(3);

        assert_eq!(boundary.try_admit(), AdmissionOutcome::Admitted);
        assert_eq!(boundary.try_admit(), AdmissionOutcome::Admitted);
        assert_eq!(boundary.try_admit(), AdmissionOutcome::Admitted);
        assert_eq!(boundary.try_admit(), AdmissionOutcome::QueueFull);
        assert_eq!(boundary.pending(), 3);
        assert_eq!(boundary.dropped(), 1);

        boundary.release(2);

        assert_eq!(boundary.pending(), 1);
        assert_eq!(boundary.try_admit(), AdmissionOutcome::Admitted);
        assert_eq!(boundary.try_admit(), AdmissionOutcome::Admitted);
        assert_eq!(boundary.try_admit(), AdmissionOutcome::QueueFull);
        assert_eq!(boundary.pending(), 3);
        assert_eq!(boundary.dropped(), 2);
    }

    #[test]
    fn concurrent_admission_never_exceeds_capacity() {
        const CAPACITY: usize = 8;
        const CONTENDERS: usize = 64;
        let boundary = Arc::new(boundary(CAPACITY));
        let barrier = Arc::new(Barrier::new(CONTENDERS));
        let handles: Vec<_> = (0..CONTENDERS)
            .map(|_| {
                let boundary = Arc::clone(&boundary);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    boundary.try_admit()
                })
            })
            .collect();

        let admitted = handles
            .into_iter()
            .map(|handle| handle.join().expect("admission thread does not panic"))
            .filter(|outcome| *outcome == AdmissionOutcome::Admitted)
            .count();

        assert_eq!(admitted, CAPACITY);
        assert_eq!(boundary.pending(), CAPACITY);
        assert_eq!(boundary.dropped(), (CONTENDERS - CAPACITY) as u64);
    }

    #[tokio::test]
    async fn sdk_export_failures_increment_each_bounded_metric_once() {
        let (boundary, metrics) = boundary_with_metrics(1);
        let classified_failures = AtomicU64::new(0);
        for error in [
            OTelSdkError::InternalFailure("private failure".to_owned()),
            OTelSdkError::Timeout(Duration::from_secs(1)),
            OTelSdkError::AlreadyShutdown,
        ] {
            let result =
                observe_export(&boundary, &classified_failures, async { Err(error) }).await;
            assert!(result.is_err());
        }

        assert_eq!(boundary.failed_exports(), 3);
        let exposition = metrics.encode().expect("metrics encode");
        for sample in [
            "github_telemetry_export_failures_total{signal=\"trace\",reason=\"internal\"} 1",
            "github_telemetry_export_failures_total{signal=\"trace\",reason=\"timeout\"} 1",
            "github_telemetry_export_failures_total{signal=\"trace\",reason=\"shutdown\"} 1",
        ] {
            assert!(
                exposition.contains(sample),
                "missing {sample:?} in:\n{exposition}"
            );
        }
    }

    #[tokio::test]
    async fn http_classified_export_failure_is_not_counted_as_internal() {
        let (boundary, metrics) = boundary_with_metrics(1);
        let classified_failures = AtomicU64::new(0);
        let result = observe_export(&boundary, &classified_failures, async {
            boundary.observer.export_failure(
                TelemetrySignal::Trace,
                TelemetryExportFailureReason::Transport,
            );
            classified_failures.fetch_add(1, Ordering::Relaxed);
            Err(OTelSdkError::InternalFailure(
                "redacted HTTP failure".to_owned(),
            ))
        })
        .await;

        assert!(result.is_err());
        assert_eq!(boundary.failed_exports(), 1);
        let exposition = metrics.encode().expect("metrics encode");
        assert!(exposition.contains(
            "github_telemetry_export_failures_total{signal=\"trace\",reason=\"transport\"} 1"
        ));
        assert!(exposition.contains(
            "github_telemetry_export_failures_total{signal=\"trace\",reason=\"internal\"} 0"
        ));
    }

    #[test]
    fn releasing_more_than_pending_saturates_at_zero() {
        let boundary = boundary(2);
        assert_eq!(boundary.try_admit(), AdmissionOutcome::Admitted);

        boundary.release(2);

        assert_eq!(boundary.pending(), 0);
        assert_eq!(boundary.try_admit(), AdmissionOutcome::Admitted);
        assert_eq!(boundary.try_admit(), AdmissionOutcome::Admitted);
    }
}
