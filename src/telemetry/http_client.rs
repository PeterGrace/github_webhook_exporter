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
use crate::telemetry::diagnostics::DiagnosticsObserver;

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

    fn record_failure(&self, reason: TelemetryExportFailureReason) {
        self.classified_failures.fetch_add(1, Ordering::Relaxed);
        self.observer.export_failure(self.signal, reason);
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
                let reason = error.downcast_ref::<reqwest::Error>().map_or(
                    TelemetryExportFailureReason::Transport,
                    classify_reqwest_error,
                );
                self.record_failure(reason);
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
            self.record_failure(TelemetryExportFailureReason::Encoding);
            return Err(Box::new(InvalidOtlpResponse));
        }
        Ok(response)
    }
}

fn classify_reqwest_error(error: &reqwest::Error) -> TelemetryExportFailureReason {
    if error.is_timeout() {
        TelemetryExportFailureReason::Timeout
    } else if error.status().is_some() {
        TelemetryExportFailureReason::HttpResponse
    } else {
        TelemetryExportFailureReason::Transport
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
        time::Duration,
    };

    use opentelemetry_http::{Bytes, HttpClient, HttpError, Request, Response};

    use crate::{
        metrics::{Metrics, TelemetryExportFailureReason, TelemetrySignal},
        telemetry::{
            diagnostics::DiagnosticsObserver,
            http_client::{classify_reqwest_error, ObservingHttpClient},
        },
    };

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

        assert_eq!(
            classify_reqwest_error(&status),
            TelemetryExportFailureReason::HttpResponse
        );
        assert_eq!(
            classify_reqwest_error(&timeout),
            TelemetryExportFailureReason::Timeout
        );
        assert_eq!(
            classify_reqwest_error(&transport),
            TelemetryExportFailureReason::Transport
        );
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
