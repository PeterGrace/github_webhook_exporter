//! Centralized trace policy for bounded span names, statuses, and identifiers.
#![allow(dead_code)]

use opentelemetry::{trace::Status, KeyValue};
use tracing::{info_span, Span};
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::domain::{
    delivery::DeliveryId, merge_queue::PullRequestNumber, repository::RepositoryId,
};
use crate::security::CanonicalRepositoryName;

const TELEMETRY_TARGET: &str = "github_webhook_exporter";
const SQLITE_SYSTEM_NAME: &str = "sqlite";
const OPERATION_FAILURE_EVENT: &str = "operation.failure";
const OPERATION_OUTCOME_KEY: &str = "ghe.operation.outcome";
const FAILURE_REASON_KEY: &str = "ghe.failure.reason";
const REPOSITORY_NAME_KEY: &str = "github.repository.name";
const REPOSITORY_ID_KEY: &str = "github.repository.id";
const DELIVERY_ID_KEY: &str = "github.delivery.id";
const PULL_REQUEST_NUMBER_KEY: &str = "github.pull_request.number";
const COMMIT_SHA_KEY: &str = "github.commit.sha";
const DB_SYSTEM_NAME_KEY: &str = "db.system.name";
const DB_OPERATION_NAME_KEY: &str = "db.operation.name";

/// A bounded high-level operation recorded in tracing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Operation {
    /// An HTTP request boundary.
    HttpRequest,
    /// Webhook authentication and repository authorization.
    WebhookAuthenticate,
    /// Webhook body decoding and processing.
    WebhookProcess,
    /// Repository configuration persistence.
    RepositoryWrite,
    /// A SQLite query boundary.
    SqliteQuery,
    /// A merge-queue state update.
    MergeQueueUpdate,
    /// A retention job run.
    RetentionRun,
}

/// A bounded terminal outcome recorded in tracing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OperationOutcome {
    /// The operation succeeded.
    Success,
    /// The operation was a duplicate.
    Duplicate,
    /// The operation made no durable change.
    NoOp,
    /// The operation was cancelled.
    Cancelled,
    /// The operation failed.
    Failure,
}

/// A bounded SQLite operation recorded in tracing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DatabaseOperation {
    /// Counting repository rows.
    RepositoryCount,
    /// Creating a repository row.
    RepositoryCreate,
    /// Listing repository rows.
    RepositoryList,
    /// Authenticating a repository row.
    RepositoryAuthenticate,
    /// Loading one repository row.
    RepositoryGet,
    /// Updating one repository row.
    RepositoryUpdate,
    /// Deleting one repository row.
    RepositoryDelete,
    /// Claiming one delivery row.
    DeliveryClaim,
    /// Pruning processed delivery rows.
    DeliveryPrune,
    /// Enqueuing one merge-queue row.
    MergeQueueEnqueue,
    /// Completing one merge-queue row.
    MergeQueueComplete,
    /// Pruning merge-queue rows.
    MergeQueuePrune,
}

/// Creates a bounded tracing span for a high-level operation.
///
/// The span name is fixed by the operation vocabulary and never accepts caller-supplied text.
pub(crate) fn operation_span(operation: Operation) -> Span {
    match operation {
        Operation::HttpRequest => info_span!(target: TELEMETRY_TARGET, "http.request"),
        Operation::WebhookAuthenticate => {
            info_span!(target: TELEMETRY_TARGET, "github.webhook.authenticate")
        }
        Operation::WebhookProcess => {
            info_span!(target: TELEMETRY_TARGET, "github.webhook.process")
        }
        Operation::RepositoryWrite => {
            info_span!(target: TELEMETRY_TARGET, "config.repository.write")
        }
        Operation::SqliteQuery => info_span!(target: TELEMETRY_TARGET, "sqlite.query"),
        Operation::MergeQueueUpdate => {
            info_span!(target: TELEMETRY_TARGET, "merge_queue.update")
        }
        Operation::RetentionRun => {
            info_span!(target: TELEMETRY_TARGET, parent: None, "retention.run")
        }
    }
}

/// Creates a bounded tracing span for a SQLite query.
///
/// The returned span is named `sqlite.query` and is annotated only with fixed database metadata.
pub(crate) fn database_span(operation: DatabaseOperation) -> Span {
    let span = operation_span(Operation::SqliteQuery);
    span.set_attribute(DB_SYSTEM_NAME_KEY, SQLITE_SYSTEM_NAME);
    span.set_attribute(DB_OPERATION_NAME_KEY, operation.as_str());
    span
}

/// Records the canonical repository name as an OpenTelemetry span attribute.
///
/// # Parameters
///
/// * `span` - The active tracing span.
/// * `name` - The validated canonical repository name.
pub(crate) fn set_repository_name(span: &Span, name: &CanonicalRepositoryName) {
    span.set_attribute(REPOSITORY_NAME_KEY, name.as_str().to_owned());
}

/// Records the repository database identifier as an OpenTelemetry span attribute.
///
/// # Parameters
///
/// * `span` - The active tracing span.
/// * `id` - The validated positive repository identifier.
pub(crate) fn set_repository_id(span: &Span, id: RepositoryId) {
    span.set_attribute(REPOSITORY_ID_KEY, id.get());
}

/// Records the GitHub delivery identifier as an OpenTelemetry span attribute.
///
/// # Parameters
///
/// * `span` - The active tracing span.
/// * `id` - The validated delivery UUID.
pub(crate) fn set_delivery_id(span: &Span, id: &DeliveryId) {
    let mut buffer = uuid::Uuid::encode_buffer();
    span.set_attribute(DELIVERY_ID_KEY, id.encode_lower(&mut buffer).to_owned());
}

/// Records the pull-request number as an OpenTelemetry span attribute.
///
/// # Parameters
///
/// * `span` - The active tracing span.
/// * `number` - The validated positive pull-request number.
pub(crate) fn set_pull_request_number(span: &Span, number: PullRequestNumber) {
    span.set_attribute(PULL_REQUEST_NUMBER_KEY, number.get());
}

/// Records the Git commit SHA as an OpenTelemetry span attribute.
///
/// # Parameters
///
/// * `span` - The active tracing span.
/// * `sha` - The validated commit SHA.
pub(crate) fn set_commit_sha(span: &Span, sha: &str) {
    span.set_attribute(COMMIT_SHA_KEY, sha.to_owned());
}

/// Records the bounded terminal outcome and maps failure to an error status.
///
/// # Parameters
///
/// * `span` - The active tracing span.
/// * `outcome` - The bounded operation outcome.
pub(crate) fn set_status(span: &Span, outcome: OperationOutcome) {
    span.set_attribute(OPERATION_OUTCOME_KEY, outcome.as_str());
    span.set_status(match outcome {
        OperationOutcome::Failure => Status::error("operation_failed"),
        OperationOutcome::Success
        | OperationOutcome::Duplicate
        | OperationOutcome::NoOp
        | OperationOutcome::Cancelled => Status::Ok,
    });
}

/// Adds a bounded failure event to the active span.
///
/// # Parameters
///
/// * `span` - The active tracing span.
/// * `reason` - A fixed failure reason from the bounded telemetry vocabulary.
pub(crate) fn add_failure_event(span: &Span, reason: &'static str) {
    span.add_event(
        OPERATION_FAILURE_EVENT,
        vec![KeyValue::new(FAILURE_REASON_KEY, reason)],
    );
}

impl Operation {
    /// Returns the fixed span name for this operation.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::HttpRequest => "http.request",
            Self::WebhookAuthenticate => "github.webhook.authenticate",
            Self::WebhookProcess => "github.webhook.process",
            Self::RepositoryWrite => "config.repository.write",
            Self::SqliteQuery => "sqlite.query",
            Self::MergeQueueUpdate => "merge_queue.update",
            Self::RetentionRun => "retention.run",
        }
    }
}

impl OperationOutcome {
    /// Returns the fixed span outcome value.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Duplicate => "duplicate",
            Self::NoOp => "no_op",
            Self::Cancelled => "cancelled",
            Self::Failure => "failure",
        }
    }
}

impl DatabaseOperation {
    /// Returns the fixed database-operation name.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::RepositoryCount => "repository.count",
            Self::RepositoryCreate => "repository.create",
            Self::RepositoryList => "repository.list",
            Self::RepositoryAuthenticate => "repository.authenticate",
            Self::RepositoryGet => "repository.get",
            Self::RepositoryUpdate => "repository.update",
            Self::RepositoryDelete => "repository.delete",
            Self::DeliveryClaim => "delivery.claim",
            Self::DeliveryPrune => "delivery.prune",
            Self::MergeQueueEnqueue => "merge_queue.enqueue",
            Self::MergeQueueComplete => "merge_queue.complete",
            Self::MergeQueuePrune => "merge_queue.prune",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    use opentelemetry::trace::{Status, TracerProvider as _};
    use opentelemetry_sdk::error::OTelSdkResult;
    use opentelemetry_sdk::trace::{
        SdkTracerProvider, SimpleSpanProcessor, SpanData, SpanExporter,
    };
    use tracing::Dispatch;
    use tracing_subscriber::{fmt::format::FmtSpan, layer::SubscriberExt, registry::Registry};

    use crate::domain::{
        delivery::DeliveryId, merge_queue::PullRequestNumber, repository::RepositoryId,
    };
    use crate::metrics::{
        Action, EventType, MergeGroupReason, MergeQueueOutcome, MergeQueueReason,
    };
    use crate::security::CanonicalRepositoryName;

    use super::{
        add_failure_event, database_span, operation_span, set_commit_sha, set_delivery_id,
        set_pull_request_number, set_repository_id, set_repository_name, set_status,
        DatabaseOperation, Operation, OperationOutcome, COMMIT_SHA_KEY, DB_OPERATION_NAME_KEY,
        DB_SYSTEM_NAME_KEY, DELIVERY_ID_KEY, FAILURE_REASON_KEY, OPERATION_FAILURE_EVENT,
        OPERATION_OUTCOME_KEY, PULL_REQUEST_NUMBER_KEY, REPOSITORY_ID_KEY, REPOSITORY_NAME_KEY,
        SQLITE_SYSTEM_NAME,
    };

    const TEST_DELIVERY_ID: &str = "550e8400-e29b-41d4-a716-446655440000";
    const TEST_COMMIT_SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    const TEST_REPOSITORY_NAME: &str = "Owner/Private-Repository";

    #[derive(Clone, Default, Debug)]
    struct CollectingSpanExporter(Arc<Mutex<Vec<SpanData>>>);

    impl CollectingSpanExporter {
        fn finished_spans(&self) -> Vec<SpanData> {
            self.0
                .lock()
                .expect("span capture lock is available")
                .clone()
        }
    }

    impl SpanExporter for CollectingSpanExporter {
        async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
            self.0
                .lock()
                .expect("span capture lock is available")
                .extend(batch);
            Ok(())
        }
    }

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

    fn test_subscriber(
        writer: SharedWriter,
    ) -> (Dispatch, CollectingSpanExporter, SdkTracerProvider) {
        let exporter = CollectingSpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_span_processor(SimpleSpanProcessor::new(exporter.clone()))
            .build();
        let tracer = provider.tracer("github_webhook_exporter");
        let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
        let subscriber = Registry::default()
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
                    .with_writer(writer),
            )
            .with(otel_layer);
        (Dispatch::new(subscriber), exporter, provider)
    }

    #[test]
    fn bounded_values_remain_fixed_for_every_variant() {
        let operations = [
            (Operation::HttpRequest, "http.request"),
            (
                Operation::WebhookAuthenticate,
                "github.webhook.authenticate",
            ),
            (Operation::WebhookProcess, "github.webhook.process"),
            (Operation::RepositoryWrite, "config.repository.write"),
            (Operation::SqliteQuery, "sqlite.query"),
            (Operation::MergeQueueUpdate, "merge_queue.update"),
            (Operation::RetentionRun, "retention.run"),
        ];
        for (operation, expected) in operations {
            assert_eq!(operation.as_str(), expected);
        }

        let outcomes = [
            (OperationOutcome::Success, "success"),
            (OperationOutcome::Duplicate, "duplicate"),
            (OperationOutcome::NoOp, "no_op"),
            (OperationOutcome::Cancelled, "cancelled"),
            (OperationOutcome::Failure, "failure"),
        ];
        for (outcome, expected) in outcomes {
            assert_eq!(outcome.as_str(), expected);
        }

        let database_operations = [
            (DatabaseOperation::RepositoryCount, "repository.count"),
            (DatabaseOperation::RepositoryCreate, "repository.create"),
            (DatabaseOperation::RepositoryList, "repository.list"),
            (
                DatabaseOperation::RepositoryAuthenticate,
                "repository.authenticate",
            ),
            (DatabaseOperation::RepositoryGet, "repository.get"),
            (DatabaseOperation::RepositoryUpdate, "repository.update"),
            (DatabaseOperation::RepositoryDelete, "repository.delete"),
            (DatabaseOperation::DeliveryClaim, "delivery.claim"),
            (DatabaseOperation::DeliveryPrune, "delivery.prune"),
            (DatabaseOperation::MergeQueueEnqueue, "merge_queue.enqueue"),
            (
                DatabaseOperation::MergeQueueComplete,
                "merge_queue.complete",
            ),
            (DatabaseOperation::MergeQueuePrune, "merge_queue.prune"),
        ];
        for (operation, expected) in database_operations {
            assert_eq!(operation.as_str(), expected);
        }
    }

    #[test]
    fn operation_spans_keep_sensitive_identifiers_out_of_fmt_output_and_export_otlp_attributes() {
        let repository_name = CanonicalRepositoryName::new(TEST_REPOSITORY_NAME)
            .expect("test repository name is valid");
        let delivery_id = DeliveryId::parse(TEST_DELIVERY_ID).expect("test delivery UUID is valid");
        let repository_id = RepositoryId::new(42).expect("test repository id is positive");
        let pull_request_number =
            PullRequestNumber::new(17).expect("test pull request number is positive");
        let output = SharedWriter::default();
        let (dispatch, exporter, provider) = test_subscriber(output.clone());

        tracing::dispatcher::with_default(&dispatch, || {
            let request_span = operation_span(Operation::WebhookProcess);
            set_repository_name(&request_span, &repository_name);
            set_repository_id(&request_span, repository_id);
            set_delivery_id(&request_span, &delivery_id);
            set_pull_request_number(&request_span, pull_request_number);
            set_commit_sha(&request_span, TEST_COMMIT_SHA);
            set_status(&request_span, OperationOutcome::Failure);
            add_failure_event(&request_span, "missing_active_attempt");
            drop(request_span);

            let database = database_span(DatabaseOperation::RepositoryCreate);
            drop(database);
        });

        provider.force_flush().expect("spans flush");
        let spans = exporter.finished_spans();
        assert_eq!(spans.len(), 2);

        let request_span = spans
            .iter()
            .find(|span| span.name.as_ref() == "github.webhook.process")
            .expect("request span is exported");
        let database_span = spans
            .iter()
            .find(|span| span.name.as_ref() == "sqlite.query")
            .expect("database span is exported");

        assert!(request_span.attributes.iter().any(|attribute| {
            attribute.key.as_str() == REPOSITORY_NAME_KEY
                && attribute.value.as_str().as_ref() == repository_name.as_str()
        }));
        assert!(request_span.attributes.iter().any(|attribute| {
            attribute.key.as_str() == REPOSITORY_ID_KEY && attribute.value.as_str().as_ref() == "42"
        }));
        assert!(request_span.attributes.iter().any(|attribute| {
            attribute.key.as_str() == DELIVERY_ID_KEY
                && attribute.value.as_str().as_ref() == TEST_DELIVERY_ID
        }));
        assert!(request_span.attributes.iter().any(|attribute| {
            attribute.key.as_str() == PULL_REQUEST_NUMBER_KEY
                && attribute.value.as_str().as_ref() == "17"
        }));
        assert!(request_span.attributes.iter().any(|attribute| {
            attribute.key.as_str() == COMMIT_SHA_KEY
                && attribute.value.as_str().as_ref() == TEST_COMMIT_SHA
        }));
        assert!(request_span.attributes.iter().any(|attribute| {
            attribute.key.as_str() == OPERATION_OUTCOME_KEY
                && attribute.value.as_str().as_ref() == OperationOutcome::Failure.as_str()
        }));
        assert!(request_span.events.events.iter().any(|event| {
            event.name.as_ref() == OPERATION_FAILURE_EVENT
                && event.attributes.iter().any(|attribute| {
                    attribute.key.as_str() == FAILURE_REASON_KEY
                        && attribute.value.as_str().as_ref() == "missing_active_attempt"
                })
        }));
        assert!(
            matches!(request_span.status, Status::Error { ref description } if description.as_ref() == "operation_failed")
        );

        assert!(database_span.attributes.iter().any(|attribute| {
            attribute.key.as_str() == DB_SYSTEM_NAME_KEY
                && attribute.value.as_str().as_ref() == SQLITE_SYSTEM_NAME
        }));
        assert!(database_span.attributes.iter().any(|attribute| {
            attribute.key.as_str() == DB_OPERATION_NAME_KEY
                && attribute.value.as_str().as_ref() == DatabaseOperation::RepositoryCreate.as_str()
        }));

        let stderr = output.contents();
        assert!(!stderr.contains("owner/private-repository"));
        assert!(!stderr.contains(TEST_DELIVERY_ID));
        assert!(!stderr.contains(TEST_COMMIT_SHA));
    }

    #[test]
    fn bounded_metrics_enums_remain_closed_to_untrusted_input() {
        assert_eq!(EventType::PullRequest.as_str(), "pull_request");
        assert_eq!(Action::Opened.as_str(), "opened");
        assert_eq!(MergeGroupReason::Merged.as_str(), "merged");
        assert_eq!(MergeQueueOutcome::Succeeded.as_str(), "succeeded");
        assert_eq!(
            MergeQueueReason::PullRequestMerged.as_str(),
            "pull_request_merged"
        );
    }
}
