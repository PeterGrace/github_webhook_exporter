use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::{future::Future, sync::Arc, time::Duration};

use opentelemetry::{logs::Severity, Context, InstrumentationScope};
use opentelemetry_sdk::{
    error::OTelSdkResult,
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

#[derive(Debug)]
pub(super) struct AdmissionBoundary {
    capacity: usize,
    pending: AtomicUsize,
    dropped: AtomicU64,
    failed_exports: AtomicU64,
}

impl AdmissionBoundary {
    pub(super) fn new(capacity: usize) -> Self {
        debug_assert!(capacity > 0);
        Self {
            capacity,
            pending: AtomicUsize::new(0),
            dropped: AtomicU64::new(0),
            failed_exports: AtomicU64::new(0),
        }
    }

    pub(super) fn try_admit(&self) -> bool {
        let mut pending = self.pending.load(Ordering::Acquire);
        loop {
            if pending >= self.capacity {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                return false;
            }
            match self.pending.compare_exchange_weak(
                pending,
                pending + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(observed) => pending = observed,
            }
        }
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
    export: impl Future<Output = OTelSdkResult>,
) -> OTelSdkResult {
    let result = export.await;
    if result.is_err() {
        boundary.failed_exports.fetch_add(1, Ordering::Relaxed);
    }
    result
}

#[derive(Debug)]
struct BoundarySpanExporter<E> {
    exporter: E,
    boundary: Arc<AdmissionBoundary>,
}

impl<E: SpanExporter> SpanExporter for BoundarySpanExporter<E> {
    fn export(
        &self,
        batch: Vec<SpanData>,
    ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
        self.boundary.release(batch.len());
        observe_export(&self.boundary, self.exporter.export(batch))
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.exporter.shutdown_with_timeout(timeout)
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.exporter.force_flush()
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
        if self.boundary.try_admit() {
            self.processor.on_end(span);
        }
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.processor.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.processor.shutdown_with_timeout(timeout)
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.processor.set_resource(resource);
    }
}

pub(super) fn span_processor<E: SpanExporter + 'static>(
    exporter: E,
    capacity: usize,
    batch_size: usize,
) -> (BoundarySpanProcessor, Arc<AdmissionBoundary>) {
    let boundary = Arc::new(AdmissionBoundary::new(capacity));
    let exporter = BoundarySpanExporter {
        exporter,
        boundary: Arc::clone(&boundary),
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
}

impl<E: LogExporter> LogExporter for BoundaryLogExporter<E> {
    fn export(
        &self,
        batch: LogBatch<'_>,
    ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
        self.boundary.release(batch.iter().count());
        observe_export(&self.boundary, self.exporter.export(batch))
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.exporter.shutdown_with_timeout(timeout)
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
        if self.boundary.try_admit() {
            self.processor.emit(record, scope);
        }
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.processor.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.processor.shutdown_with_timeout(timeout)
    }

    fn event_enabled(&self, level: Severity, target: &str, name: Option<&str>) -> bool {
        self.processor.event_enabled(level, target, name)
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.processor.set_resource(resource);
    }
}

pub(super) fn log_processor<E: LogExporter + 'static>(
    exporter: E,
    capacity: usize,
    batch_size: usize,
) -> (BoundaryLogProcessor, Arc<AdmissionBoundary>) {
    let boundary = Arc::new(AdmissionBoundary::new(capacity));
    let exporter = BoundaryLogExporter {
        exporter,
        boundary: Arc::clone(&boundary),
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
        sync::{Arc, Barrier},
        thread,
    };

    use opentelemetry_sdk::error::OTelSdkError;

    use super::{observe_export, AdmissionBoundary};

    #[test]
    fn admission_capacity_is_exact_and_released_batches_restore_space() {
        let boundary = AdmissionBoundary::new(3);

        assert!(boundary.try_admit());
        assert!(boundary.try_admit());
        assert!(boundary.try_admit());
        assert!(!boundary.try_admit());
        assert_eq!(boundary.pending(), 3);
        assert_eq!(boundary.dropped(), 1);

        boundary.release(2);

        assert_eq!(boundary.pending(), 1);
        assert!(boundary.try_admit());
        assert!(boundary.try_admit());
        assert!(!boundary.try_admit());
        assert_eq!(boundary.pending(), 3);
        assert_eq!(boundary.dropped(), 2);
    }

    #[test]
    fn concurrent_admission_never_exceeds_capacity() {
        const CAPACITY: usize = 8;
        const CONTENDERS: usize = 64;
        let boundary = Arc::new(AdmissionBoundary::new(CAPACITY));
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
            .filter(|admitted| *admitted)
            .count();

        assert_eq!(admitted, CAPACITY);
        assert_eq!(boundary.pending(), CAPACITY);
        assert_eq!(boundary.dropped(), (CONTENDERS - CAPACITY) as u64);
    }

    #[tokio::test]
    async fn export_failures_are_counted_by_the_application_hook() {
        let boundary = AdmissionBoundary::new(1);

        let result = observe_export(&boundary, async {
            Err(OTelSdkError::InternalFailure("private failure".to_owned()))
        })
        .await;

        assert!(result.is_err());
        assert_eq!(boundary.failed_exports(), 1);
    }

    #[test]
    fn releasing_more_than_pending_saturates_at_zero() {
        let boundary = AdmissionBoundary::new(2);
        assert!(boundary.try_admit());

        boundary.release(2);

        assert_eq!(boundary.pending(), 0);
        assert!(boundary.try_admit());
        assert!(boundary.try_admit());
    }
}
