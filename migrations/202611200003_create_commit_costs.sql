-- Finalized run totals of the runs a commit triggered (`pull_request`, `pull_request_target`,
-- `push` and `merge_group`), keyed by run attempt so a re-finalized attempt replaces its row. The
-- rows of a commit are deleted once its record is final; its state row then answers whether a late
-- run can still change the total.
CREATE TABLE commit_run_costs (
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    workflow_run_id INTEGER NOT NULL CHECK (workflow_run_id > 0),
    workflow_run_attempt INTEGER NOT NULL CHECK (workflow_run_attempt > 0),
    commit_sha TEXT NOT NULL CHECK (
        length(commit_sha) IN (40, 64) AND commit_sha NOT GLOB '*[^0-9a-f]*'
    ),
    event TEXT NOT NULL CHECK (
        event IN ('pull_request', 'pull_request_target', 'push', 'merge_group')
    ),
    pull_request_number INTEGER CHECK (pull_request_number IS NULL OR pull_request_number > 0),
    total_usd REAL NOT NULL CHECK (total_usd >= 0),
    PRIMARY KEY (repository_id, workflow_run_id, workflow_run_attempt)
);

CREATE INDEX commit_run_costs_commit_idx
    ON commit_run_costs(repository_id, commit_sha);

-- Each commit run total split by cost source. Like the pull-request split it references only
-- `repositories(id)`; the store deletes a split with its total.
CREATE TABLE commit_run_cost_sources (
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    workflow_run_id INTEGER NOT NULL CHECK (workflow_run_id > 0),
    workflow_run_attempt INTEGER NOT NULL CHECK (workflow_run_attempt > 0),
    source TEXT NOT NULL CHECK (
        length(source) BETWEEN 1 AND 32 AND source NOT GLOB '*[^a-z0-9_-]*'
    ),
    cost_usd REAL NOT NULL CHECK (cost_usd >= 0),
    PRIMARY KEY (repository_id, workflow_run_id, workflow_run_attempt, source)
);

-- One row per commit with cost activity. A row is pending until `finalized_at` is set; the
-- finalized row stays for the retention period so a late run is recognized as late.
CREATE TABLE commit_costs (
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    commit_sha TEXT NOT NULL CHECK (
        length(commit_sha) IN (40, 64) AND commit_sha NOT GLOB '*[^0-9a-f]*'
    ),
    last_activity_at TEXT NOT NULL,
    finalized_at TEXT,
    partial INTEGER CHECK (partial IS NULL OR partial IN (0, 1)),
    total_usd REAL CHECK (total_usd IS NULL OR total_usd >= 0),
    CHECK ((finalized_at IS NULL) = (partial IS NULL)),
    CHECK ((finalized_at IS NULL) = (total_usd IS NULL)),
    PRIMARY KEY (repository_id, commit_sha)
);

CREATE INDEX commit_costs_pending_idx
    ON commit_costs(last_activity_at) WHERE finalized_at IS NULL;
CREATE INDEX commit_costs_last_activity_at_idx
    ON commit_costs(last_activity_at);

-- Commits with a run total finalized before commit totals began. Those earlier runs were never
-- stored per commit, so a record for one of these commits is partial. The set is written once and
-- pruned on the commit cost clock.
CREATE TABLE commit_costs_before_rollup (
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    commit_sha TEXT NOT NULL CHECK (
        length(commit_sha) IN (40, 64) AND commit_sha NOT GLOB '*[^0-9a-f]*'
    ),
    updated_at TEXT NOT NULL,
    PRIMARY KEY (repository_id, commit_sha)
);

CREATE INDEX commit_costs_before_rollup_updated_at_idx
    ON commit_costs_before_rollup(updated_at);

INSERT INTO commit_costs_before_rollup (repository_id, commit_sha, updated_at)
SELECT repository_id, head_sha, MAX(updated_at)
FROM run_cost_finalizations
WHERE finalized_at IS NOT NULL AND head_sha IS NOT NULL
AND event IN ('pull_request', 'pull_request_target', 'push', 'merge_group')
GROUP BY repository_id, head_sha;

-- A commit's record counts the vendor runs on its head commit.
CREATE INDEX ci_vendor_runs_head_sha_idx
    ON ci_vendor_runs(repository_id, head_sha);
