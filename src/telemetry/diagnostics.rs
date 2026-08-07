use std::{
    fmt::Debug,
    io::{self, Write as _},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Instant,
};

use crate::metrics::{Metrics, TelemetryDropReason, TelemetryExportFailureReason, TelemetrySignal};

const REPORT_INTERVAL_MILLIS: u64 = 60_000;
const FAILURE_CATEGORY_COUNT: usize = 14;
const DROP_CATEGORY_COUNT: usize = 4;

pub(super) trait Clock: Debug + Send + Sync {
    fn now_millis(&self) -> u64;
}

pub(super) trait DiagnosticSink: Debug + Send + Sync {
    fn write(&self, line: &str) -> io::Result<()>;
}

#[derive(Debug)]
struct MonotonicClock(Instant);

impl Default for MonotonicClock {
    fn default() -> Self {
        Self(Instant::now())
    }
}

impl Clock for MonotonicClock {
    fn now_millis(&self) -> u64 {
        u64::try_from(self.0.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

#[derive(Debug)]
struct StderrSink;

impl DiagnosticSink for StderrSink {
    fn write(&self, line: &str) -> io::Result<()> {
        io::stderr().lock().write_all(line.as_bytes())
    }
}

#[derive(Debug)]
struct CategoryLimiter {
    next_report_millis: AtomicU64,
    suppressed: AtomicU64,
}

impl CategoryLimiter {
    fn new() -> Self {
        Self {
            next_report_millis: AtomicU64::new(0),
            suppressed: AtomicU64::new(0),
        }
    }

    fn claim_report(&self, now_millis: u64) -> Option<u64> {
        let mut next_report = self.next_report_millis.load(Ordering::Relaxed);
        loop {
            if now_millis < next_report {
                self.suppressed.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            let new_deadline = now_millis.saturating_add(REPORT_INTERVAL_MILLIS);
            match self.next_report_millis.compare_exchange_weak(
                next_report,
                new_deadline,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(self.suppressed.swap(0, Ordering::AcqRel)),
                Err(observed) => next_report = observed,
            }
        }
    }
}

#[derive(Debug)]
struct DiagnosticsInner {
    metrics: Metrics,
    clock: Arc<dyn Clock>,
    sink: Arc<dyn DiagnosticSink>,
    failure_limiters: [CategoryLimiter; FAILURE_CATEGORY_COUNT],
    drop_limiters: [CategoryLimiter; DROP_CATEGORY_COUNT],
}

#[derive(Clone, Debug)]
pub(super) struct DiagnosticsObserver {
    inner: Arc<DiagnosticsInner>,
}

impl DiagnosticsObserver {
    pub(super) fn new(metrics: Metrics) -> Self {
        Self::with_dependencies(
            metrics,
            Arc::new(MonotonicClock::default()),
            Arc::new(StderrSink),
        )
    }

    fn with_dependencies(
        metrics: Metrics,
        clock: Arc<dyn Clock>,
        sink: Arc<dyn DiagnosticSink>,
    ) -> Self {
        Self {
            inner: Arc::new(DiagnosticsInner {
                metrics,
                clock,
                sink,
                failure_limiters: std::array::from_fn(|_| CategoryLimiter::new()),
                drop_limiters: std::array::from_fn(|_| CategoryLimiter::new()),
            }),
        }
    }

    pub(super) fn export_failure(
        &self,
        signal: TelemetrySignal,
        reason: TelemetryExportFailureReason,
    ) {
        self.inner
            .metrics
            .record_telemetry_export_failure(signal, reason);
        let limiter = &self.inner.failure_limiters[failure_index(signal, reason)];
        self.report(limiter, "failure", signal.as_str(), reason.as_str());
    }

    pub(super) fn drop_record(&self, signal: TelemetrySignal, reason: TelemetryDropReason) {
        self.drop_records(signal, reason, 1);
    }

    pub(super) fn drop_records(
        &self,
        signal: TelemetrySignal,
        reason: TelemetryDropReason,
        count: u64,
    ) {
        if count == 0 {
            return;
        }
        self.inner
            .metrics
            .record_telemetry_drops(signal, reason, count);
        let limiter = &self.inner.drop_limiters[drop_index(signal, reason)];
        self.report(limiter, "drop", signal.as_str(), reason.as_str());
    }

    fn report(&self, limiter: &CategoryLimiter, kind: &str, signal: &str, reason: &str) {
        let Some(suppressed) = limiter.claim_report(self.inner.clock.now_millis()) else {
            return;
        };
        let line = format!(
            "telemetry pipeline diagnostic kind={kind} signal={signal} reason={reason} suppressed={suppressed}\n"
        );
        drop(self.inner.sink.write(&line));
    }
}

fn signal_index(signal: TelemetrySignal) -> usize {
    match signal {
        TelemetrySignal::Trace => 0,
        TelemetrySignal::Log => 1,
    }
}

fn failure_index(signal: TelemetrySignal, reason: TelemetryExportFailureReason) -> usize {
    let reason_index = match reason {
        TelemetryExportFailureReason::Transport => 0,
        TelemetryExportFailureReason::Timeout => 1,
        TelemetryExportFailureReason::HttpResponse => 2,
        TelemetryExportFailureReason::Encoding => 3,
        TelemetryExportFailureReason::Shutdown => 4,
        TelemetryExportFailureReason::Internal => 5,
        TelemetryExportFailureReason::Other => 6,
    };
    signal_index(signal) * TelemetryExportFailureReason::ALL.len() + reason_index
}

fn drop_index(signal: TelemetrySignal, reason: TelemetryDropReason) -> usize {
    let reason_index = match reason {
        TelemetryDropReason::QueueFull => 0,
        TelemetryDropReason::PipelineClosed => 1,
    };
    signal_index(signal) * TelemetryDropReason::ALL.len() + reason_index
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc, Barrier, Mutex,
        },
        thread,
        time::Duration,
    };

    use crate::{
        metrics::{Metrics, TelemetryDropReason, TelemetryExportFailureReason, TelemetrySignal},
        telemetry::diagnostics::{Clock, DiagnosticSink, DiagnosticsObserver},
    };

    #[derive(Debug, Default)]
    struct TestClock(AtomicU64);

    impl TestClock {
        fn advance(&self, duration: Duration) {
            self.0
                .fetch_add(duration.as_millis() as u64, Ordering::Relaxed);
        }
    }

    impl Clock for TestClock {
        fn now_millis(&self) -> u64 {
            self.0.load(Ordering::Relaxed)
        }
    }

    #[derive(Debug, Default)]
    struct CaptureSink(Mutex<Vec<String>>);

    impl CaptureSink {
        fn lines(&self) -> Vec<String> {
            self.0.lock().expect("capture lock is available").clone()
        }
    }

    impl DiagnosticSink for CaptureSink {
        fn write(&self, line: &str) -> io::Result<()> {
            self.0
                .lock()
                .expect("capture lock is available")
                .push(line.to_owned());
            Ok(())
        }
    }

    #[test]
    fn one_report_per_category_per_minute_includes_suppressed_count() {
        let metrics = Metrics::new();
        let clock = Arc::new(TestClock::default());
        let sink = Arc::new(CaptureSink::default());
        let observer =
            DiagnosticsObserver::with_dependencies(metrics.clone(), clock.clone(), sink.clone());

        for _ in 0..3 {
            observer.export_failure(
                TelemetrySignal::Trace,
                TelemetryExportFailureReason::Timeout,
            );
        }
        assert_eq!(
            sink.lines(),
            vec!["telemetry pipeline diagnostic kind=failure signal=trace reason=timeout suppressed=0\n"]
        );

        clock.advance(Duration::from_secs(60));
        observer.export_failure(
            TelemetrySignal::Trace,
            TelemetryExportFailureReason::Timeout,
        );
        assert_eq!(
            sink.lines()[1],
            "telemetry pipeline diagnostic kind=failure signal=trace reason=timeout suppressed=2\n"
        );
        assert!(metrics.encode().expect("metrics encode").contains(
            "github_telemetry_export_failures_total{signal=\"trace\",reason=\"timeout\"} 4"
        ));
    }

    #[test]
    fn concurrent_reports_emit_once_and_account_for_every_suppression() {
        const REPORTERS: usize = 32;
        let clock = Arc::new(TestClock::default());
        let sink = Arc::new(CaptureSink::default());
        let observer =
            DiagnosticsObserver::with_dependencies(Metrics::new(), clock.clone(), sink.clone());
        let barrier = Arc::new(Barrier::new(REPORTERS));
        let handles: Vec<_> = (0..REPORTERS)
            .map(|_| {
                let observer = observer.clone();
                let barrier = barrier.clone();
                thread::spawn(move || {
                    barrier.wait();
                    observer.drop_record(TelemetrySignal::Trace, TelemetryDropReason::QueueFull);
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("reporter does not panic");
        }

        assert_eq!(sink.lines().len(), 1);
        clock.advance(Duration::from_secs(60));
        observer.drop_record(TelemetrySignal::Trace, TelemetryDropReason::QueueFull);
        let lines = sink.lines();
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|line| line.starts_with(
            "telemetry pipeline diagnostic kind=drop signal=trace reason=queue_full suppressed="
        )));
        let suppressed_total: u64 = lines
            .iter()
            .map(|line| {
                line.trim_end()
                    .rsplit_once('=')
                    .expect("diagnostic includes a suppression count")
                    .1
                    .parse::<u64>()
                    .expect("suppression count is numeric")
            })
            .sum();
        assert_eq!(suppressed_total, 31);
    }

    #[test]
    fn categories_are_independent() {
        let metrics = Metrics::new();
        let sink = Arc::new(CaptureSink::default());
        let observer = DiagnosticsObserver::with_dependencies(
            metrics.clone(),
            Arc::new(TestClock::default()),
            sink.clone(),
        );

        observer.export_failure(
            TelemetrySignal::Log,
            TelemetryExportFailureReason::Transport,
        );
        observer.drop_record(TelemetrySignal::Log, TelemetryDropReason::QueueFull);

        assert_eq!(sink.lines().len(), 2);
        assert!(metrics.encode().expect("metrics encode").contains(
            "github_telemetry_dropped_records_total{signal=\"log\",reason=\"queue_full\"} 1"
        ));
    }
}
