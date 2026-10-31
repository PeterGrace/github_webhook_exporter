use async_trait::async_trait;
use opentelemetry_http::{Bytes, HttpClient, HttpError, Request, Response};
use opentelemetry_proto::tonic::collector::{
    logs::v1::ExportLogsServiceResponse, trace::v1::ExportTraceServiceResponse,
};
use prost::Message;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use thiserror::Error;

use crate::metrics::{TelemetryExportFailureReason, TelemetrySignal};
use crate::telemetry::diagnostics::{
    DiagnosticsObserver, ExportFailureContext, ExportFailureDetail,
};

#[derive(Debug)]
pub(super) struct ObservingHttpClient<C> {
    inner: C,
    signal: TelemetrySignal,
    observer: DiagnosticsObserver,
    classified_failures: Arc<AtomicU64>,
}

impl<C> ObservingHttpClient<C> {
    pub(super) fn new(inner: C, signal: TelemetrySignal, observer: DiagnosticsObserver) -> Self {
        Self {
            inner,
            signal,
            observer,
            classified_failures: Arc::new(AtomicU64::new(0)),
        }
    }

    pub(super) fn classified_failures(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.classified_failures)
    }

    fn record_failure(&self, failure: ClassifiedHttpFailure) {
        self.classified_failures.fetch_add(1, Ordering::Relaxed);
        self.observer
            .export_failure_with_context(self.signal, failure.reason, failure.context);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ClassifiedHttpFailure {
    reason: TelemetryExportFailureReason,
    context: ExportFailureContext,
}

impl ClassifiedHttpFailure {
    fn new(reason: TelemetryExportFailureReason) -> Self {
        Self {
            reason,
            context: ExportFailureContext::default(),
        }
    }

    fn with_status(reason: TelemetryExportFailureReason, status: u16) -> Self {
        Self {
            reason,
            context: ExportFailureContext::with_status(status),
        }
    }

    fn with_detail(reason: TelemetryExportFailureReason, detail: ExportFailureDetail) -> Self {
        Self {
            reason,
            context: ExportFailureContext::with_detail(detail),
        }
    }
}

#[derive(Debug, Error)]
#[error("invalid OTLP response")]
struct InvalidOtlpResponse;

#[async_trait]
impl<C: HttpClient> HttpClient for ObservingHttpClient<C> {
    async fn send_bytes(&self, request: Request<Bytes>) -> Result<Response<Bytes>, HttpError> {
        let response = match self.inner.send_bytes(request).await {
            Ok(response) => response,
            Err(error) => {
                let failure = error.downcast_ref::<reqwest::Error>().map_or_else(
                    || ClassifiedHttpFailure::new(TelemetryExportFailureReason::Transport),
                    classify_reqwest_error,
                );
                self.record_failure(failure);
                return Err(error);
            }
        };

        let valid = match self.signal {
            TelemetrySignal::Trace => {
                ExportTraceServiceResponse::decode(response.body().clone()).is_ok()
            }
            TelemetrySignal::Log => {
                ExportLogsServiceResponse::decode(response.body().clone()).is_ok()
            }
        };
        if !valid {
            self.record_failure(ClassifiedHttpFailure::new(
                TelemetryExportFailureReason::Encoding,
            ));
            return Err(Box::new(InvalidOtlpResponse));
        }
        Ok(response)
    }
}

fn classify_reqwest_error(error: &reqwest::Error) -> ClassifiedHttpFailure {
    if error.is_timeout() {
        ClassifiedHttpFailure::new(TelemetryExportFailureReason::Timeout)
    } else if let Some(status) = error.status() {
        ClassifiedHttpFailure::with_status(
            TelemetryExportFailureReason::HttpResponse,
            status.as_u16(),
        )
    } else {
        let detail = if error.is_connect() {
            ExportFailureDetail::Connect
        } else if error.is_builder() {
            ExportFailureDetail::RequestBuilder
        } else if error.is_redirect() {
            ExportFailureDetail::Redirect
        } else {
            ExportFailureDetail::Request
        };
        ClassifiedHttpFailure::with_detail(TelemetryExportFailureReason::Transport, detail)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{self, Read, Write},
        net::TcpListener,
        sync::Mutex,
        thread,
        time::{Duration, Instant},
    };

    use opentelemetry_http::{Bytes, HttpClient, HttpError, Request, Response};

    use crate::{
        metrics::{Metrics, TelemetryExportFailureReason, TelemetrySignal},
        telemetry::{
            build_blocking_http_client,
            diagnostics::{DiagnosticsObserver, ExportFailureDetail},
            http_client::{classify_reqwest_error, ClassifiedHttpFailure, ObservingHttpClient},
        },
    };

    #[derive(Debug)]
    struct ErrorClient(Mutex<Option<HttpError>>);

    #[async_trait::async_trait]
    impl HttpClient for ErrorClient {
        async fn send_bytes(&self, _request: Request<Bytes>) -> Result<Response<Bytes>, HttpError> {
            Err(self
                .0
                .lock()
                .expect("error-client lock is available")
                .take()
                .expect("one error is configured"))
        }
    }

    #[derive(Debug)]
    struct StaticResponseClient;

    #[async_trait::async_trait]
    impl HttpClient for StaticResponseClient {
        async fn send_bytes(&self, _request: Request<Bytes>) -> Result<Response<Bytes>, HttpError> {
            Ok(Response::builder()
                .status(200)
                .body(Bytes::from_static(&[0xff, 0xff]))
                .expect("response is valid"))
        }
    }

    fn request_error(response: Option<&'static [u8]>, timeout: Duration) -> reqwest::Error {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener binds");
        let address = listener
            .local_addr()
            .expect("listener address is available");
        if let Some(response) = response {
            thread::spawn(move || {
                let (mut stream, _) = listener.accept().expect("test request arrives");
                let mut request = [0_u8; 1024];
                let _ = stream.read(&mut request);
                if response.is_empty() {
                    thread::sleep(Duration::from_millis(100));
                } else {
                    stream.write_all(response).expect("test response writes");
                }
            });
        } else {
            drop(listener);
        }
        reqwest::blocking::Client::builder()
            .timeout(timeout)
            .build()
            .expect("client builds")
            .get(format!("http://{address}"))
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .expect_err("request fails")
    }

    #[test]
    fn reqwest_failures_use_structured_bounded_classification() {
        let status = request_error(
            Some(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n"),
            Duration::from_secs(1),
        );
        let timeout = request_error(Some(b""), Duration::from_millis(10));
        let transport = request_error(None, Duration::from_secs(1));
        let builder = reqwest::blocking::Client::new()
            .get("ftp://collector.invalid/v1/logs")
            .send()
            .expect_err("unsupported transport fails request construction");
        let request = request_error(Some(b"not an HTTP response"), Duration::from_secs(1));

        assert_eq!(
            classify_reqwest_error(&status),
            ClassifiedHttpFailure::with_status(TelemetryExportFailureReason::HttpResponse, 503,)
        );
        assert_eq!(
            classify_reqwest_error(&timeout),
            ClassifiedHttpFailure::new(TelemetryExportFailureReason::Timeout)
        );
        assert_eq!(
            classify_reqwest_error(&transport),
            ClassifiedHttpFailure::with_detail(
                TelemetryExportFailureReason::Transport,
                ExportFailureDetail::Connect,
            )
        );
        assert_eq!(
            classify_reqwest_error(&builder),
            ClassifiedHttpFailure::with_detail(
                TelemetryExportFailureReason::Transport,
                ExportFailureDetail::RequestBuilder,
            )
        );
        assert_eq!(
            classify_reqwest_error(&request),
            ClassifiedHttpFailure::with_detail(
                TelemetryExportFailureReason::Transport,
                ExportFailureDetail::Request,
            )
        );
    }

    #[test]
    fn production_client_rejects_redirects_without_contacting_the_target() {
        let redirect_target = TcpListener::bind("127.0.0.1:0").expect("target listener binds");
        redirect_target
            .set_nonblocking(true)
            .expect("target listener becomes nonblocking");
        let target_address = redirect_target
            .local_addr()
            .expect("target listener address is available");
        let redirect_source = TcpListener::bind("127.0.0.1:0").expect("source listener binds");
        let source_address = redirect_source
            .local_addr()
            .expect("source listener address is available");
        thread::spawn(move || {
            let (mut stream, _) = redirect_source.accept().expect("test request arrives");
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request);
            let response = format!(
                "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{target_address}/\r\n\
                 Content-Length: 0\r\n\r\n"
            );
            stream
                .write_all(response.as_bytes())
                .expect("redirect response writes");
        });
        let client = build_blocking_http_client(Duration::from_secs(1))
            .expect("production HTTP client builds");
        let error = client
            .get(format!("http://{source_address}"))
            .header("x-api-key", "test-exporter-credential")
            .send()
            .expect_err("production redirect policy rejects response");

        assert_eq!(
            classify_reqwest_error(&error),
            ClassifiedHttpFailure::with_detail(
                TelemetryExportFailureReason::Transport,
                ExportFailureDetail::Redirect,
            )
        );
        let deadline = Instant::now() + Duration::from_millis(100);
        loop {
            match redirect_target.accept() {
                Ok(_) => panic!("redirect target must not receive a request"),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        break;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("target listener failed: {error}"),
            }
        }
    }

    #[tokio::test]
    async fn send_bytes_records_each_structured_reqwest_failure() {
        let failures = tokio::task::spawn_blocking(|| {
            [
                (
                    request_error(
                        Some(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n"),
                        Duration::from_secs(1),
                    ),
                    "http_response",
                ),
                (
                    request_error(Some(b""), Duration::from_millis(10)),
                    "timeout",
                ),
                (request_error(None, Duration::from_secs(1)), "transport"),
            ]
        })
        .await
        .expect("failure fixtures are created");

        for (error, reason) in failures {
            let metrics = Metrics::new();
            let client = ObservingHttpClient::new(
                ErrorClient(Mutex::new(Some(Box::new(error)))),
                TelemetrySignal::Log,
                DiagnosticsObserver::new(metrics.clone()),
            );
            let request = Request::builder()
                .uri("http://collector.invalid/v1/logs")
                .body(Bytes::new())
                .expect("request is valid");

            assert!(client.send_bytes(request).await.is_err());
            let sample = format!(
                "github_telemetry_export_failures_total{{signal=\"log\",reason=\"{reason}\"}} 1"
            );
            let exposition = metrics.encode().expect("metrics encode");
            assert!(
                exposition.contains(&sample),
                "missing {sample:?} in:\n{exposition}"
            );
        }
    }

    #[tokio::test]
    async fn malformed_success_response_is_an_encoding_failure() {
        let metrics = Metrics::new();
        let client = ObservingHttpClient::new(
            StaticResponseClient,
            TelemetrySignal::Trace,
            DiagnosticsObserver::new(metrics.clone()),
        );
        let request = Request::builder()
            .uri("http://collector.invalid/v1/traces")
            .body(Bytes::new())
            .expect("request is valid");

        let result = client.send_bytes(request).await;

        assert!(result.is_err());
        assert!(metrics.encode().expect("metrics encode").contains(
            "github_telemetry_export_failures_total{signal=\"trace\",reason=\"encoding\"} 1"
        ));
    }
}
