-- The head commit of each finalized run total, so a pull request's record can count its commits.
ALTER TABLE pull_request_run_costs ADD COLUMN head_sha TEXT CHECK (
    head_sha IS NULL OR (length(head_sha) IN (40, 64) AND head_sha NOT GLOB '*[^0-9a-f]*')
);

UPDATE pull_request_run_costs SET head_sha = (
    SELECT finalization.head_sha FROM run_cost_finalizations AS finalization
    WHERE finalization.repository_id = pull_request_run_costs.repository_id
    AND finalization.workflow_run_id = pull_request_run_costs.workflow_run_id
    AND finalization.workflow_run_attempt = pull_request_run_costs.workflow_run_attempt
);

-- Each finalized run total split by cost source, so a pull request's record can carry one
-- `ci.cost.<source>_usd` per source after the run's job links and usage entries are pruned.
CREATE TABLE pull_request_run_cost_sources (
    repository_id INTEGER NOT NULL,
    pull_request_number INTEGER NOT NULL,
    workflow_run_id INTEGER NOT NULL,
    workflow_run_attempt INTEGER NOT NULL,
    source TEXT NOT NULL CHECK (
        length(source) BETWEEN 1 AND 32 AND source NOT GLOB '*[^a-z0-9_-]*'
    ),
    cost_usd REAL NOT NULL CHECK (cost_usd >= 0),
    PRIMARY KEY (
        repository_id, pull_request_number, workflow_run_id, workflow_run_attempt, source
    ),
    FOREIGN KEY (repository_id, pull_request_number, workflow_run_id, workflow_run_attempt)
        REFERENCES pull_request_run_costs (
            repository_id, pull_request_number, workflow_run_id, workflow_run_attempt
        ) ON DELETE CASCADE
);

-- Run totals finalized before this migration are split from the job links and usage entries that
-- retention still holds.
INSERT INTO pull_request_run_cost_sources (
    repository_id, pull_request_number, workflow_run_id, workflow_run_attempt, source, cost_usd
)
SELECT repository_id, pull_request_number, workflow_run_id, workflow_run_attempt, source,
       TOTAL(cost_usd)
FROM (
    SELECT run.repository_id, run.pull_request_number, run.workflow_run_id,
           run.workflow_run_attempt, 'github_actions' AS source, job.cost_usd
    FROM pull_request_run_costs AS run
    JOIN workflow_job_links AS job
        ON job.repository_id = run.repository_id
        AND job.workflow_run_id = run.workflow_run_id
        AND job.workflow_run_attempt = run.workflow_run_attempt
    WHERE job.cost_usd IS NOT NULL
    UNION ALL
    SELECT run.repository_id, run.pull_request_number, run.workflow_run_id,
           run.workflow_run_attempt, usage.source, usage.cost_usd
    FROM pull_request_run_costs AS run
    JOIN ci_usage_entries AS usage
        ON usage.repository_id = run.repository_id
        AND usage.workflow_run_id = run.workflow_run_id
        AND usage.workflow_run_attempt = run.workflow_run_attempt
)
GROUP BY repository_id, pull_request_number, workflow_run_id, workflow_run_attempt, source;

-- One row per pull request with cost activity. A row is pending until `finalized_at` is set; the
-- finalized row stays for the retention period so a late run is recognized as late.
CREATE TABLE pull_request_costs (
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    pull_request_number INTEGER NOT NULL CHECK (pull_request_number > 0),
    created_at TEXT,
    first_activity_at TEXT NOT NULL,
    last_activity_at TEXT NOT NULL,
    outcome TEXT NOT NULL DEFAULT 'open' CHECK (outcome IN ('open', 'merged', 'closed')),
    merge_commit_sha TEXT CHECK (
        merge_commit_sha IS NULL
        OR (length(merge_commit_sha) IN (40, 64) AND merge_commit_sha NOT GLOB '*[^0-9a-f]*')
    ),
    closed_at TEXT,
    post_merge_activity_at TEXT,
    finalized_at TEXT,
    finalized_by TEXT CHECK (
        finalized_by IS NULL
        OR finalized_by IN ('post_merge_run', 'timeout', 'closed_grace', 'max_age')
    ),
    partial INTEGER CHECK (partial IS NULL OR partial IN (0, 1)),
    total_usd REAL CHECK (total_usd IS NULL OR total_usd >= 0),
    CHECK ((finalized_at IS NULL) = (finalized_by IS NULL)),
    CHECK ((finalized_at IS NULL) = (partial IS NULL)),
    CHECK ((finalized_at IS NULL) = (total_usd IS NULL)),
    CHECK ((outcome = 'open') = (closed_at IS NULL)),
    PRIMARY KEY (repository_id, pull_request_number)
);

CREATE INDEX pull_request_costs_pending_idx
    ON pull_request_costs(last_activity_at) WHERE finalized_at IS NULL;
CREATE INDEX pull_request_costs_last_activity_at_idx
    ON pull_request_costs(last_activity_at);

-- Pull requests with run totals or vendor runs stored before this migration start pending, so they
-- are finalized and their stored costs are kept until then. Their creation time is unknown until a
-- `pull_request` delivery records it, so until then they are partial.
INSERT INTO pull_request_costs (
    repository_id, pull_request_number, first_activity_at, last_activity_at
)
SELECT repository_id, pull_request_number, MIN(updated_at), MAX(updated_at)
FROM (
    SELECT repository_id, pull_request_number, updated_at FROM pull_request_run_costs
    UNION ALL
    SELECT repository_id, pull_request_number, updated_at FROM ci_vendor_runs
)
GROUP BY repository_id, pull_request_number;

-- Finalization waits while a run of the pull request, or a run on its merge commit, is still
-- pending; these keep that check from scanning every schedule of the repository.
CREATE INDEX run_cost_finalizations_pending_pull_request_idx
    ON run_cost_finalizations(repository_id, pull_request_number)
    WHERE finalized_at IS NULL AND abandoned_at IS NULL;
CREATE INDEX run_cost_finalizations_pending_head_sha_idx
    ON run_cost_finalizations(repository_id, head_sha)
    WHERE finalized_at IS NULL AND abandoned_at IS NULL;

-- When run totals began, written once. A pull request created earlier is marked partial, since
-- its earlier runs were never totalled. Run totals that already exist date it.
CREATE TABLE pull_request_cost_rollup (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    started_at TEXT NOT NULL
);

INSERT INTO pull_request_cost_rollup (id, started_at)
SELECT 1, MIN(updated_at) FROM run_cost_finalizations HAVING MIN(updated_at) IS NOT NULL;
