-- The workflow definition's own name, from a `workflow_run` delivery's top-level `workflow`. A run
-- with a `run-name` reports that run name in `workflow_run.name` and `workflow_job.workflow_name`,
-- so job spans read the definition's name from here instead.
ALTER TABLE workflow_run_contexts ADD COLUMN workflow_name TEXT CHECK (
    workflow_name IS NULL OR length(workflow_name) BETWEEN 1 AND 128
);
