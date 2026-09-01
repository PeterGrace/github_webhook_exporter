CREATE TABLE required_check_contexts (
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    target_branch TEXT NOT NULL CHECK (length(target_branch) BETWEEN 1 AND 255),
    check_names TEXT NOT NULL CHECK (length(check_names) <= 16384),
    updated_at TEXT NOT NULL,
    PRIMARY KEY (repository_id, target_branch)
);

CREATE INDEX required_check_contexts_updated_at_idx
    ON required_check_contexts(updated_at);
