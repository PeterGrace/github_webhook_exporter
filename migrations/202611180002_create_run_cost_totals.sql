-- SQLite cannot relax NOT NULL, so job links are rebuilt to keep a job's billing even when no
-- job trace was exported, and to record the price-table label the job was billed under.
CREATE TABLE workflow_job_links_rebuilt (
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
    trace_id TEXT CHECK (trace_id IS NULL OR length(trace_id) = 32),
    span_id TEXT CHECK (span_id IS NULL OR length(span_id) = 16),
    started_at_nanos INTEGER NOT NULL,
    completed_at_nanos INTEGER NOT NULL CHECK (completed_at_nanos >= started_at_nanos),
    timing_source TEXT NOT NULL CHECK (timing_source IN ('reported', 'fallback')),
    updated_at TEXT NOT NULL,
    billable_minutes INTEGER CHECK (billable_minutes IS NULL OR billable_minutes >= 0),
    cost_usd REAL CHECK (cost_usd IS NULL OR cost_usd >= 0),
    price_table_version TEXT CHECK (
        price_table_version IS NULL OR length(price_table_version) = 12
    ),
    runner_label TEXT CHECK (
        runner_label IS NULL
        OR (length(runner_label) BETWEEN 1 AND 32 AND runner_label NOT GLOB '*[^a-z0-9_]*')
    ),
    CHECK ((trace_id IS NULL) = (span_id IS NULL)),
    PRIMARY KEY (repository_id, workflow_run_id, workflow_run_attempt, workflow_job_id)
);

INSERT INTO workflow_job_links_rebuilt (
    repository_id, workflow_run_id, workflow_run_attempt, workflow_job_id, job_name, conclusion,
    trace_id, span_id, started_at_nanos, completed_at_nanos, timing_source, updated_at,
    billable_minutes, cost_usd, price_table_version
)
SELECT
    repository_id, workflow_run_id, workflow_run_attempt, workflow_job_id, job_name, conclusion,
    trace_id, span_id, started_at_nanos, completed_at_nanos, timing_source, updated_at,
    billable_minutes, cost_usd, price_table_version
FROM workflow_job_links;

DROP TABLE workflow_job_links;
ALTER TABLE workflow_job_links_rebuilt RENAME TO workflow_job_links;
CREATE INDEX workflow_job_links_updated_at_idx
    ON workflow_job_links(updated_at);

-- One row per completed run attempt. A row is pending until `finalized_at` is set, so a restart
-- during the grace period loses nothing, and a finalized row answers whether a late job or
-- usage post can still change the total. A failed total pushes `due_at` back, and a row that no
-- longer parses is set `abandoned_at` and never polled again.
CREATE TABLE run_cost_finalizations (
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    workflow_run_id INTEGER NOT NULL CHECK (workflow_run_id > 0),
    workflow_run_attempt INTEGER NOT NULL CHECK (workflow_run_attempt > 0),
    event TEXT NOT NULL CHECK (
        event IN (
            'pull_request', 'pull_request_target', 'pull_request_review',
            'pull_request_review_comment', 'issue_comment', 'merge_group', 'push', 'schedule',
            'workflow_dispatch', 'workflow_run', 'other'
        )
    ),
    workflow_name TEXT CHECK (workflow_name IS NULL OR length(workflow_name) BETWEEN 1 AND 128),
    workflow_file TEXT CHECK (workflow_file IS NULL OR length(workflow_file) BETWEEN 1 AND 128),
    conclusion TEXT NOT NULL CHECK (
        conclusion IN (
            'success', 'failure', 'cancelled', 'skipped', 'timed_out', 'neutral', 'other'
        )
    ),
    head_sha TEXT CHECK (
        head_sha IS NULL
        OR (length(head_sha) IN (40, 64) AND head_sha NOT GLOB '*[^0-9a-f]*')
    ),
    pull_request_number INTEGER CHECK (pull_request_number IS NULL OR pull_request_number > 0),
    pull_request_resolution TEXT NOT NULL CHECK (
        pull_request_resolution IN (
            'pull_requests', 'merge_queue_branch', 'run_name', 'commit_map', 'none'
        )
    ),
    due_at TEXT NOT NULL,
    failed_attempts INTEGER NOT NULL DEFAULT 0 CHECK (failed_attempts >= 0),
    abandoned_at TEXT,
    finalized_at TEXT,
    total_usd REAL CHECK (total_usd IS NULL OR total_usd >= 0),
    updated_at TEXT NOT NULL,
    CHECK ((finalized_at IS NULL) = (total_usd IS NULL)),
    CHECK (finalized_at IS NULL OR abandoned_at IS NULL),
    CHECK ((pull_request_number IS NULL) = (pull_request_resolution = 'none')),
    PRIMARY KEY (repository_id, workflow_run_id, workflow_run_attempt)
);

CREATE INDEX run_cost_finalizations_pending_idx
    ON run_cost_finalizations(due_at) WHERE finalized_at IS NULL AND abandoned_at IS NULL;
CREATE INDEX run_cost_finalizations_updated_at_idx
    ON run_cost_finalizations(updated_at);

-- Finalized run totals keyed by the pull request they were attributed to.
CREATE TABLE pull_request_run_costs (
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    pull_request_number INTEGER NOT NULL CHECK (pull_request_number > 0),
    workflow_run_id INTEGER NOT NULL CHECK (workflow_run_id > 0),
    workflow_run_attempt INTEGER NOT NULL CHECK (workflow_run_attempt > 0),
    total_usd REAL NOT NULL CHECK (total_usd >= 0),
    price_table_version TEXT CHECK (
        price_table_version IS NULL OR length(price_table_version) = 12
    ),
    updated_at TEXT NOT NULL,
    PRIMARY KEY (repository_id, pull_request_number, workflow_run_id, workflow_run_attempt)
);

CREATE INDEX pull_request_run_costs_updated_at_idx
    ON pull_request_run_costs(updated_at);

-- Commits seen on `pull_request` deliveries, so a run whose payload names no pull request can
-- still be attributed by its head commit.
CREATE TABLE pull_request_commits (
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    commit_sha TEXT NOT NULL CHECK (
        length(commit_sha) IN (40, 64) AND commit_sha NOT GLOB '*[^0-9a-f]*'
    ),
    pull_request_number INTEGER NOT NULL CHECK (pull_request_number > 0),
    updated_at TEXT NOT NULL,
    PRIMARY KEY (repository_id, commit_sha)
);

CREATE INDEX pull_request_commits_updated_at_idx
    ON pull_request_commits(updated_at);
