-- SQLite cannot alter a CHECK constraint, so the run-context table is rebuilt to widen `event`.
CREATE TABLE workflow_run_contexts_widened (
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    workflow_run_id INTEGER NOT NULL CHECK (workflow_run_id > 0),
    workflow_run_attempt INTEGER NOT NULL CHECK (workflow_run_attempt > 0),
    event TEXT NOT NULL CHECK (
        event IN (
            'pull_request', 'pull_request_target', 'pull_request_review',
            'pull_request_review_comment', 'issue_comment', 'merge_group', 'push', 'schedule',
            'workflow_dispatch', 'other'
        )
    ),
    source_branch TEXT CHECK (
        source_branch IS NULL OR length(source_branch) BETWEEN 1 AND 255
    ),
    target_branch TEXT CHECK (
        target_branch IS NULL OR length(target_branch) BETWEEN 1 AND 255
    ),
    pull_request_number INTEGER CHECK (pull_request_number IS NULL OR pull_request_number > 0),
    workflow_file TEXT CHECK (workflow_file IS NULL OR length(workflow_file) BETWEEN 1 AND 128),
    updated_at TEXT NOT NULL,
    PRIMARY KEY (repository_id, workflow_run_id, workflow_run_attempt)
);

INSERT INTO workflow_run_contexts_widened (
    repository_id, workflow_run_id, workflow_run_attempt, event, source_branch, target_branch,
    updated_at
)
SELECT
    repository_id, workflow_run_id, workflow_run_attempt, event, source_branch, target_branch,
    updated_at
FROM workflow_run_contexts;

DROP TABLE workflow_run_contexts;
ALTER TABLE workflow_run_contexts_widened RENAME TO workflow_run_contexts;
CREATE INDEX workflow_run_contexts_updated_at_idx
    ON workflow_run_contexts(updated_at);

ALTER TABLE workflow_job_links ADD COLUMN billable_minutes INTEGER
    CHECK (billable_minutes IS NULL OR billable_minutes >= 0);
ALTER TABLE workflow_job_links ADD COLUMN cost_usd REAL
    CHECK (cost_usd IS NULL OR cost_usd >= 0);
ALTER TABLE workflow_job_links ADD COLUMN price_table_version TEXT
    CHECK (price_table_version IS NULL OR length(price_table_version) = 12);
