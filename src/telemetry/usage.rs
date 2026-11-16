//! In-job usage reports: inference and Daytona spend that no webhook carries.

use std::time::SystemTime;

use opentelemetry::{
    trace::{Span as _, SpanKind, Tracer},
    Context, KeyValue,
};
use tracing::info;

use crate::{
    domain::merge_queue::PullRequestNumber,
    security::oidc::GitHubActionsClaims,
    telemetry::{
        trace::{
            github_workflow_attribute, job_workflow_file_attribute, price_table_version_attribute,
            pull_request_numbers_attribute, repository_name_attribute, sandbox_usage_attributes,
            sentry_description_attribute, sentry_operation_attribute, token_count_attributes,
            usage_cost_attributes, usage_job_attribute, usage_label_attributes,
            workflow_event_attribute, workflow_file_attribute, workflow_run_attempt_attribute,
            CI_SOURCE_DAYTONA, CI_USAGE_OPERATION,
        },
        workflow::{
            append_pipeline_and_repository_context, DisplayName, WorkflowFile,
            WorkflowTraceEmitter, UNKNOWN_WORKFLOW_NAME,
        },
    },
};

const MAX_SOURCE_LENGTH: usize = 32;
const USAGE_SPAN_NAME: &str = "ci.usage";

/// A caller-named spend source such as `anthropic`: 1 to 32 lowercase ASCII letters, digits,
/// `_` or `-`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UsageSource(String);

impl UsageSource {
    /// Validates a source name, returning `None` for anything outside the allowed characters.
    pub(crate) fn parse(value: &str) -> Option<Self> {
        let valid = (1..=MAX_SOURCE_LENGTH).contains(&value.len())
            && value.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
            });
        valid.then(|| Self(value.to_owned()))
    }

    /// Returns the source name.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns whether this entry is Daytona sandbox usage, priced by the exporter.
    pub(crate) fn is_daytona(&self) -> bool {
        self.0 == CI_SOURCE_DAYTONA
    }
}

/// What one entry measured.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum UsageMeasure {
    /// Model inference, priced in USD by the caller.
    Inference {
        input_tokens: Option<i64>,
        output_tokens: Option<i64>,
    },
    /// Daytona sandbox time, priced by the exporter.
    Sandbox {
        cpu_seconds: f64,
        ram_gb_seconds: f64,
    },
}

/// One accepted, priced usage entry.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct UsageEntry {
    pub(crate) source: UsageSource,
    pub(crate) task: Option<DisplayName>,
    pub(crate) model: Option<DisplayName>,
    pub(crate) origin: Option<DisplayName>,
    pub(crate) cost_usd: f64,
    /// Whether the exporter had no rate for this entry and recorded it at $0.
    pub(crate) fell_back: bool,
    pub(crate) price_table_version: Option<String>,
    pub(crate) measure: UsageMeasure,
}

/// One accepted usage post: verified attribution plus its priced entries.
#[derive(Clone, Debug)]
pub(crate) struct UsageReport {
    pub(crate) claims: GitHubActionsClaims,
    pub(crate) job: Option<DisplayName>,
    pub(crate) pull_request: Option<PullRequestNumber>,
    pub(crate) received_at: SystemTime,
    pub(crate) entries: Vec<UsageEntry>,
}

impl UsageReport {
    fn workflow_name(&self) -> &str {
        self.claims
            .workflow
            .as_ref()
            .map_or(UNKNOWN_WORKFLOW_NAME, DisplayName::as_str)
    }

    fn pull_requests(&self) -> &[PullRequestNumber] {
        self.pull_request.as_slice()
    }
}

impl WorkflowTraceEmitter {
    /// Emits one independent, instantaneous `ci.usage` span per entry of `report`.
    pub(crate) fn emit_usage(&self, report: &UsageReport) {
        let Some(tracer) = self.tracer() else {
            return;
        };
        for entry in &report.entries {
            let mut span = tracer.build_with_context(
                tracer
                    .span_builder(USAGE_SPAN_NAME)
                    .with_kind(SpanKind::Internal)
                    .with_start_time(report.received_at)
                    .with_attributes(usage_attributes(report, entry)),
                &Context::new(),
            );
            span.end_with_timestamp(report.received_at);
        }
    }
}

fn usage_attributes(report: &UsageReport, entry: &UsageEntry) -> Vec<KeyValue> {
    let claims = &report.claims;
    let mut attributes = Vec::with_capacity(24);
    append_pipeline_and_repository_context(
        &mut attributes,
        report.workflow_name(),
        &claims.repository,
        claims.run_id,
        claims.sha.as_ref(),
    );
    attributes.push(repository_name_attribute(&claims.repository));
    attributes.push(workflow_run_attempt_attribute(claims.run_attempt));
    attributes.push(workflow_event_attribute(claims.event));
    attributes.push(github_workflow_attribute(report.workflow_name()));
    if let Some(file) = &claims.workflow_file {
        attributes.push(workflow_file_attribute(file));
    }
    if let Some(file) = &claims.job_workflow_file {
        attributes.push(job_workflow_file_attribute(file));
    }
    if let Some(pull_requests) = pull_request_numbers_attribute(report.pull_requests()) {
        attributes.push(pull_requests);
    }
    if let Some(job) = &report.job {
        attributes.push(usage_job_attribute(job));
    }
    attributes.extend(usage_cost_attributes(
        entry.source.as_str(),
        entry.cost_usd,
        entry.fell_back,
    ));
    if let Some(version) = &entry.price_table_version {
        attributes.push(price_table_version_attribute(version));
    }
    attributes.extend(usage_label_attributes(
        entry.task.as_ref(),
        entry.model.as_ref(),
        entry.origin.as_ref(),
    ));
    match entry.measure {
        UsageMeasure::Inference {
            input_tokens,
            output_tokens,
        } => attributes.extend(token_count_attributes(input_tokens, output_tokens)),
        UsageMeasure::Sandbox {
            cpu_seconds,
            ram_gb_seconds,
        } => attributes.extend(sandbox_usage_attributes(cpu_seconds, ram_gb_seconds)),
    }
    attributes.push(sentry_operation_attribute(CI_USAGE_OPERATION));
    attributes.push(sentry_description_attribute(format!(
        "{} {}",
        entry.source.as_str(),
        entry
            .task
            .as_ref()
            .or(report.job.as_ref())
            .map_or(report.workflow_name(), DisplayName::as_str)
    )));
    attributes
}

/// Logs one `ci cost recorded` line per entry of `report`, with the attributes of its span.
///
/// The log is independent of span export, so usage cost reaches the logs dataset even when the
/// trace emitter is off.
pub(crate) fn log_usage_costs(report: &UsageReport) {
    let claims = &report.claims;
    let _parentless_context = Context::new().attach();
    for entry in &report.entries {
        let (input_tokens, output_tokens, cpu_seconds, ram_gb_seconds) = match entry.measure {
            UsageMeasure::Inference {
                input_tokens,
                output_tokens,
            } => (input_tokens, output_tokens, None, None),
            UsageMeasure::Sandbox {
                cpu_seconds,
                ram_gb_seconds,
            } => (None, None, Some(cpu_seconds), Some(ram_gb_seconds)),
        };
        info!(
            parent: None,
            ci.source = entry.source.as_str(),
            ci.cost_usd = entry.cost_usd,
            ci.price_table.version = entry.price_table_version.as_deref(),
            ci.price.fell_back = entry.fell_back,
            ci.job = report.job.as_ref().map(DisplayName::as_str),
            ci.task = entry.task.as_ref().map(DisplayName::as_str),
            ci.model = entry.model.as_ref().map(DisplayName::as_str),
            ci.origin = entry.origin.as_ref().map(DisplayName::as_str),
            ci.input_tokens = input_tokens,
            ci.output_tokens = output_tokens,
            ci.cpu_seconds = cpu_seconds,
            ci.ram_gb_seconds = ram_gb_seconds,
            ci.job_workflow_file = claims.job_workflow_file.as_ref().map(WorkflowFile::as_str),
            github.workflow = report.workflow_name(),
            github.workflow_file = claims.workflow_file.as_ref().map(WorkflowFile::as_str),
            github.workflow.event = claims.event.as_str(),
            github.pull_request.number = report.pull_request.map(|number| number.get()),
            github.repository = claims.repository.as_str(),
            cicd.pipeline.run.id = claims.run_id.get(),
            github.workflow.run.attempt = claims.run_attempt.get(),
            "ci cost recorded"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::UsageSource;

    #[test]
    fn sources_are_short_lowercase_tokens() {
        for valid in ["anthropic", "daytona", "open-router", "gpt_proxy2"] {
            assert!(UsageSource::parse(valid).is_some(), "{valid}");
        }
        for invalid in ["", "Anthropic", "a b", "x".repeat(33).as_str(), "ünicode"] {
            assert!(UsageSource::parse(invalid).is_none(), "{invalid}");
        }
        assert!(UsageSource::parse("daytona")
            .expect("valid source")
            .is_daytona());
    }
}
