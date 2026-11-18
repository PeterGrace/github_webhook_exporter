-- One row per priced vendor run attributed to exactly one pull request. Vendor runs have no
-- workflow run, so they are keyed by pull request instead of joining `ci_usage_entries`.
CREATE TABLE ci_vendor_runs (
    id INTEGER PRIMARY KEY,
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    pull_request_number INTEGER NOT NULL CHECK (pull_request_number > 0),
    head_sha TEXT CHECK (
        head_sha IS NULL
        OR (length(head_sha) IN (40, 64) AND head_sha NOT GLOB '*[^0-9a-f]*')
    ),
    vendor TEXT NOT NULL CHECK (length(vendor) BETWEEN 1 AND 128),
    trigger TEXT NOT NULL CHECK (trigger IN ('check_suite', 'pull_request_review')),
    cost_usd REAL NOT NULL CHECK (cost_usd >= 0),
    price_table_version TEXT NOT NULL CHECK (length(price_table_version) = 12),
    updated_at TEXT NOT NULL
);

CREATE INDEX ci_vendor_runs_pull_request_idx
    ON ci_vendor_runs(repository_id, pull_request_number);
CREATE INDEX ci_vendor_runs_updated_at_idx
    ON ci_vendor_runs(updated_at);
