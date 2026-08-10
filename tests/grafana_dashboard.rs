use serde_json::Value;

const DASHBOARD: &str = include_str!("../examples/grafana/github-webhook-exporter.json");
const REQUIRED_METRICS: [&str; 15] = [
    "github_webhook_requests_total",
    "github_webhook_events_total",
    "github_webhook_processing_duration_seconds",
    "github_webhook_request_body_bytes",
    "github_webhook_duplicates_total",
    "github_webhook_processing_failures_total",
    "github_repository_configurations",
    "github_merge_group_events_total",
    "github_merge_queue_pr_outcomes_total",
    "github_merge_queue_attempt_duration_seconds",
    "github_merge_queue_transition_failures_total",
    "github_workflow_job_steps",
    "github_workflow_job_trace_rejections_total",
    "github_telemetry_export_failures_total",
    "github_telemetry_dropped_records_total",
];
const REQUIRED_ROWS: [&str; 4] = [
    "Operational overview",
    "Webhook details",
    "Merge queue details",
    "Workflow and telemetry details",
];

fn dashboard() -> Value {
    serde_json::from_str(DASHBOARD).expect("example Grafana dashboard must be valid JSON")
}

fn collect_targets<'a>(value: &'a Value, targets: &mut Vec<&'a Value>) {
    match value {
        Value::Array(values) => values
            .iter()
            .for_each(|nested| collect_targets(nested, targets)),
        Value::Object(object) => {
            if let Some(Value::Array(values)) = object.get("targets") {
                targets.extend(values);
            }
            object
                .values()
                .for_each(|nested| collect_targets(nested, targets));
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

#[test]
fn dashboard_has_stable_identity_variables_and_rows() {
    let dashboard = dashboard();

    assert_eq!(dashboard["title"], "GitHub Webhook Exporter");
    assert_eq!(dashboard["uid"], "github-webhook-exporter");
    assert!(
        dashboard["schemaVersion"].as_u64().unwrap_or_default() >= 39,
        "dashboard must target Grafana schema version 39 or newer"
    );

    let variables = dashboard["templating"]["list"]
        .as_array()
        .expect("dashboard must define a templating list");
    for name in ["datasource", "job", "instance"] {
        assert!(
            variables.iter().any(|variable| variable["name"] == name),
            "missing template variable {name}"
        );
    }
    for name in ["job", "instance"] {
        let variable = variables
            .iter()
            .find(|variable| variable["name"] == name)
            .expect("checked variable must exist");
        assert_eq!(
            variable["multi"], true,
            "{name} must support multiple values"
        );
        assert_eq!(variable["includeAll"], true, "{name} must support All");
    }

    let row_titles: Vec<&str> = dashboard["panels"]
        .as_array()
        .expect("dashboard must define panels")
        .iter()
        .filter(|panel| panel["type"] == "row")
        .filter_map(|panel| panel["title"].as_str())
        .collect();
    assert_eq!(row_titles, REQUIRED_ROWS);
}

#[test]
fn every_prometheus_target_is_filtered_and_uses_the_selected_datasource() {
    let dashboard = dashboard();
    let mut targets = Vec::new();
    collect_targets(&dashboard["panels"], &mut targets);
    assert!(
        !targets.is_empty(),
        "dashboard must define Prometheus targets"
    );

    for target in targets {
        let expression = target["expr"]
            .as_str()
            .expect("every dashboard target must define a PromQL expression");
        assert_eq!(
            target["datasource"]["uid"], "${datasource}",
            "target must use the selected Prometheus datasource: {expression}"
        );
        assert!(
            expression.contains("job=~\"$job\""),
            "target must filter by job: {expression}"
        );
        assert!(
            expression.contains("instance=~\"$instance\""),
            "target must filter by instance: {expression}"
        );
    }
}

#[test]
fn dashboard_queries_cover_every_emitted_metric_family() {
    let dashboard = dashboard();
    let mut targets = Vec::new();
    collect_targets(&dashboard["panels"], &mut targets);
    let expressions = targets
        .iter()
        .filter_map(|target| target["expr"].as_str())
        .collect::<Vec<_>>()
        .join("\n");

    for metric in REQUIRED_METRICS {
        assert!(
            expressions.contains(metric),
            "dashboard queries do not cover {metric}"
        );
    }
}
