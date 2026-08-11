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
const GLOBAL_METRICS: [&str; 3] = [
    "github_repository_configurations",
    "github_telemetry_export_failures_total",
    "github_telemetry_dropped_records_total",
];
const REQUIRED_ROWS: [&str; 4] = [
    "Operational overview",
    "Webhook details",
    "Merge queue details",
    "Workflow and telemetry details",
];
const REQUIRED_GROUPINGS: [(&str, &str); 10] = [
    ("github_webhook_requests_total", "sum by (result)"),
    ("github_webhook_events_total", "sum by (event_type, action)"),
    (
        "github_webhook_processing_duration_seconds_bucket",
        "sum by (le, result)",
    ),
    ("github_webhook_processing_failures_total", "sum by (stage)"),
    ("github_merge_group_events_total", "sum by (action, reason)"),
    (
        "github_merge_queue_pr_outcomes_total",
        "sum by (outcome, reason)",
    ),
    (
        "github_merge_queue_attempt_duration_seconds_bucket",
        "sum by (le, outcome)",
    ),
    (
        "github_workflow_job_trace_rejections_total",
        "sum by (reason)",
    ),
    (
        "github_telemetry_export_failures_total",
        "sum by (signal, reason)",
    ),
    (
        "github_telemetry_dropped_records_total",
        "sum by (signal, reason)",
    ),
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
    let variable_names: Vec<&str> = variables
        .iter()
        .filter_map(|variable| variable["name"].as_str())
        .collect();
    assert_eq!(
        variable_names,
        ["datasource", "job", "instance", "repository"],
        "variables must follow their query dependency order"
    );
    for name in ["job", "instance", "repository"] {
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
    let repository_query = variables
        .iter()
        .find(|variable| variable["name"] == "repository")
        .and_then(|variable| variable["query"]["query"].as_str())
        .expect("repository variable must define a query");
    assert!(
        repository_query.contains("github_webhook_requests_total"),
        "repository values must come from a repository-labelled metric"
    );
    for filter in ["job=~\"$job\"", "instance=~\"$instance\""] {
        assert!(
            repository_query.contains(filter),
            "repository query must include dependent filter {filter}"
        );
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

        let uses_global_metric = GLOBAL_METRICS
            .iter()
            .any(|metric| expression.contains(metric));
        assert_eq!(
            expression.contains("repository=~\"$repository\""),
            !uses_global_metric,
            "repository filtering must match metric scope: {expression}"
        );
    }
}

#[test]
fn stat_panels_explicitly_display_the_latest_non_null_value() {
    let dashboard = dashboard();
    let stat_panels: Vec<&Value> = dashboard["panels"]
        .as_array()
        .expect("dashboard must define panels")
        .iter()
        .filter(|panel| panel["type"] == "stat")
        .collect();
    assert!(!stat_panels.is_empty(), "dashboard must define stat panels");

    for panel in stat_panels {
        let reduce_options = &panel["options"]["reduceOptions"];
        assert_eq!(
            reduce_options["values"], false,
            "stat panel must reduce its time series: {}",
            panel["title"]
        );
        assert_eq!(
            reduce_options["calcs"].as_array().map(Vec::len),
            Some(1),
            "stat panel must define exactly one reducer: {}",
            panel["title"]
        );
        assert_eq!(
            reduce_options["calcs"][0], "lastNotNull",
            "stat panel must display the latest non-null value: {}",
            panel["title"]
        );
    }
}

#[test]
fn dashboard_queries_use_the_emitted_grouping_labels() {
    let dashboard = dashboard();
    let mut targets = Vec::new();
    collect_targets(&dashboard["panels"], &mut targets);

    for (metric, grouping) in REQUIRED_GROUPINGS {
        assert!(
            targets.iter().any(|target| {
                target["expr"].as_str().is_some_and(|expression| {
                    expression.contains(metric) && expression.contains(grouping)
                })
            }),
            "dashboard has no {metric} query with {grouping}"
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
