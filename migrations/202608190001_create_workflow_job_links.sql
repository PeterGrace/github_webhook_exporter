CREATE TABLE workflow_job_links (
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    workflow_run_id INTEGER NOT NULL CHECK (workflow_run_id > 0),
    workflow_run_attempt INTEGER NOT NULL CHECK (workflow_run_attempt > 0),
    workflow_job_id INTEGER NOT NULL CHECK (workflow_job_id > 0),
    job_name TEXT CHECK (job_name IS NULL OR length(job_name) BETWEEN 1 AND 128),
    conclusion TEXT NOT NULL CHECK (
        conclusion IN (
            'success', 'failure', 'cancelled', 'skipped', 'timed_out', 'neutral', 'other'
        )
    ),
    trace_id TEXT NOT NULL CHECK (length(trace_id) = 32),
    span_id TEXT NOT NULL CHECK (length(span_id) = 16),
    started_at_nanos INTEGER NOT NULL,
    completed_at_nanos INTEGER NOT NULL CHECK (completed_at_nanos >= started_at_nanos),
    timing_source TEXT NOT NULL CHECK (timing_source IN ('reported', 'fallback')),
    updated_at TEXT NOT NULL,
    PRIMARY KEY (repository_id, workflow_run_id, workflow_run_attempt, workflow_job_id)
);

CREATE INDEX workflow_job_links_updated_at_idx
    ON workflow_job_links(updated_at);
