CREATE TABLE workflow_run_contexts (
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    workflow_run_id INTEGER NOT NULL CHECK (workflow_run_id > 0),
    workflow_run_attempt INTEGER NOT NULL CHECK (workflow_run_attempt > 0),
    event TEXT NOT NULL CHECK (event IN ('pull_request', 'merge_group', 'push', 'other')),
    source_branch TEXT CHECK (
        source_branch IS NULL OR length(source_branch) BETWEEN 1 AND 255
    ),
    target_branch TEXT CHECK (
        target_branch IS NULL OR length(target_branch) BETWEEN 1 AND 255
    ),
    updated_at TEXT NOT NULL,
    PRIMARY KEY (repository_id, workflow_run_id, workflow_run_attempt)
);

CREATE INDEX workflow_run_contexts_updated_at_idx
    ON workflow_run_contexts(updated_at);
