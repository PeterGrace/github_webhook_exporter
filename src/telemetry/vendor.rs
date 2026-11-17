//! Per-run third-party vendor charges, priced from `costs.vendors`.

use std::time::SystemTime;

use opentelemetry::{
    trace::{Span as _, SpanKind, Tracer},
    Context, KeyValue,
};
use tracing::info;

use crate::{
    domain::merge_queue::PullRequestNumber,
    security::CanonicalRepositoryName,
    telemetry::{
        trace::{
            attributed_pull_request_attribute, price_table_version_attribute,
            pull_request_numbers_attribute, repository_name_attribute,
            sentry_description_attribute, sentry_operation_attribute, vendor_run_attributes,
            workflow_head_revision_attribute, workflow_repository_name_attribute,
            workflow_repository_url_attribute, CommitSha, CI_SOURCE_VENDOR,
            CI_VENDOR_RUN_OPERATION,
        },
        workflow::{DisplayName, WorkflowTraceEmitter},
    },
};

const VENDOR_RUN_SPAN_NAME: &str = "ci.vendor.run";

/// The webhook that reported a vendor run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VendorTrigger {
    /// A `check_suite` delivery with action `completed`, matched by app slug.
    CheckSuite,
    /// A `pull_request_review` delivery with action `submitted`, matched by reviewer login.
    PullRequestReview,
}

impl VendorTrigger {
    /// Returns the bounded `ci.vendor.trigger` value, which is also the stored `trigger` column.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::CheckSuite => "check_suite",
            Self::PullRequestReview => "pull_request_review",
        }
    }
}

/// One matched, priced vendor run.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct VendorRun {
    pub(crate) repository_name: CanonicalRepositoryName,
    /// The configured vendor name, sanitized like other display names.
    pub(crate) vendor: DisplayName,
    pub(crate) trigger: VendorTrigger,
    pub(crate) cost_usd: f64,
    pub(crate) price_table_version: String,
    /// The single pull request the run is attributed to; `None` when the payload named none or
    /// several.
    pub(crate) pull_request: Option<PullRequestNumber>,
    pub(crate) head_sha: Option<CommitSha>,
    pub(crate) received_at: SystemTime,
}

impl WorkflowTraceEmitter {
    /// Emits one instantaneous, parentless `ci.vendor.run` span for `run`.
    pub(crate) fn emit_vendor_run(&self, run: &VendorRun) {
        let Some(tracer) = self.tracer() else {
            return;
        };
        let mut span = tracer.build_with_context(
            tracer
                .span_builder(VENDOR_RUN_SPAN_NAME)
                .with_kind(SpanKind::Internal)
                .with_start_time(run.received_at)
                .with_attributes(vendor_run_span_attributes(run)),
            &Context::new(),
        );
        span.end_with_timestamp(run.received_at);
    }
}

fn vendor_run_span_attributes(run: &VendorRun) -> Vec<KeyValue> {
    let mut attributes = Vec::with_capacity(13);
    attributes.extend(vendor_run_attributes(
        run.vendor.as_str(),
        run.trigger.as_str(),
        run.cost_usd,
    ));
    attributes.push(price_table_version_attribute(&run.price_table_version));
    attributes.push(repository_name_attribute(&run.repository_name));
    attributes.push(workflow_repository_url_attribute(&run.repository_name));
    attributes.push(workflow_repository_name_attribute(&run.repository_name));
    if let Some(head_sha) = &run.head_sha {
        attributes.push(workflow_head_revision_attribute(head_sha));
    }
    let pull_requests = run.pull_request.as_slice();
    attributes.extend(pull_request_numbers_attribute(pull_requests));
    attributes.extend(attributed_pull_request_attribute(pull_requests));
    attributes.push(sentry_operation_attribute(CI_VENDOR_RUN_OPERATION));
    attributes.push(sentry_description_attribute(format!(
        "{} {}",
        run.vendor.as_str(),
        run.trigger.as_str()
    )));
    attributes
}

/// Logs `ci cost recorded` for one vendor run, with the attributes of its span.
///
/// Vendor runs have no workflow run, so the log is parentless with no trace context. It is
/// independent of span export, so vendor cost reaches the logs dataset even when the trace
/// emitter is off.
pub(crate) fn log_vendor_run_cost(run: &VendorRun) {
    let _parentless_context = Context::new().attach();
    info!(
        parent: None,
        ci.source = CI_SOURCE_VENDOR,
        ci.vendor = run.vendor.as_str(),
        ci.vendor.trigger = run.trigger.as_str(),
        ci.cost_usd = run.cost_usd,
        ci.price_table.version = run.price_table_version.as_str(),
        github.pull_request.number = run.pull_request.map(PullRequestNumber::get),
        github.repository = run.repository_name.as_str(),
        vcs.ref.head.revision = run.head_sha.as_ref().map(CommitSha::as_str),
        "ci cost recorded"
    );
}
