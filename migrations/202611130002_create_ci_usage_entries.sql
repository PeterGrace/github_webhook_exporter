CREATE TABLE ci_usage_entries (
    id INTEGER PRIMARY KEY,
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    workflow_run_id INTEGER NOT NULL CHECK (workflow_run_id > 0),
    workflow_run_attempt INTEGER NOT NULL CHECK (workflow_run_attempt > 0),
    source TEXT NOT NULL CHECK (
        length(source) BETWEEN 1 AND 32 AND source NOT GLOB '*[^a-z0-9_-]*'
    ),
    job TEXT CHECK (job IS NULL OR length(job) BETWEEN 1 AND 128),
    task TEXT CHECK (task IS NULL OR length(task) BETWEEN 1 AND 128),
    model TEXT CHECK (model IS NULL OR length(model) BETWEEN 1 AND 128),
    origin TEXT CHECK (origin IS NULL OR length(origin) BETWEEN 1 AND 128),
    cost_usd REAL NOT NULL CHECK (cost_usd >= 0),
    price_fell_back INTEGER NOT NULL CHECK (price_fell_back IN (0, 1)),
    price_table_version TEXT CHECK (
        price_table_version IS NULL OR length(price_table_version) = 12
    ),
    input_tokens INTEGER CHECK (input_tokens IS NULL OR input_tokens >= 0),
    output_tokens INTEGER CHECK (output_tokens IS NULL OR output_tokens >= 0),
    cpu_seconds REAL CHECK (cpu_seconds IS NULL OR cpu_seconds BETWEEN 0 AND 1000000000),
    ram_gb_seconds REAL CHECK (ram_gb_seconds IS NULL OR ram_gb_seconds BETWEEN 0 AND 1000000000),
    updated_at TEXT NOT NULL
);

CREATE INDEX ci_usage_entries_run_idx
    ON ci_usage_entries(repository_id, workflow_run_id, workflow_run_attempt);
CREATE INDEX ci_usage_entries_updated_at_idx
    ON ci_usage_entries(updated_at);
