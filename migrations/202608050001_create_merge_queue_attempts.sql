CREATE TABLE merge_queue_attempts (
    id INTEGER PRIMARY KEY,
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    pull_request_number INTEGER NOT NULL CHECK (pull_request_number > 0),
    enqueued_at TEXT NOT NULL,
    completed_at TEXT,
    outcome TEXT NOT NULL CHECK (
        outcome IN ('pending', 'succeeded', 'failed', 'cancelled', 'unknown')
    ),
    reason_code TEXT NOT NULL,
    CHECK (
        (outcome = 'pending' AND completed_at IS NULL) OR
        (outcome <> 'pending' AND completed_at IS NOT NULL)
    )
);

CREATE UNIQUE INDEX one_active_merge_queue_attempt
    ON merge_queue_attempts(repository_id, pull_request_number)
    WHERE completed_at IS NULL;

CREATE INDEX merge_queue_attempts_completed_at_idx
    ON merge_queue_attempts(completed_at);
