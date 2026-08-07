# PR #39 Workflow Job Step-Limit Final Review Fix Report

## Scope and outcome

This wave addressed all requested final-review findings for the bounded completed workflow-job
trace behavior. The implementation behavior was already correct. The change adds regression
coverage, removes two obsolete metric method keep-alives, adds the required timestamped
changelog, and records one test-only privacy assertion correction discovered during validation.
No production correction was required.

## Finding-to-test mapping

### Finding 1: specialized exclusion and malformed-admission coverage

- Direct malformed admission coverage was added in `src/api/workflow_job.rs`:
  - `admission_rejects_malformed_json_bytes` calls `inspect_completed_job` with malformed bytes.
  - `admission_rejects_missing_workflow_job` rejects an absent wrapper member.
  - `admission_rejects_null_workflow_job` rejects a null wrapper member.
  - `admission_rejects_wrong_type_workflow_job_envelopes` rejects string, array, and numeric
    `workflow_job` values.
- Specialized exclusion integration coverage was added in
  `src/telemetry/otlp_test.rs`:
  - `unauthorized_and_non_completed_over_limit_jobs_skip_specialized_processing` sends an actual
    invalid HMAC signature for the unauthorized branch and a valid authenticated `in_progress`
    request for the non-completed branch. It asserts `401` plus no claim for unauthorized, `204`
    plus one claim for non-completed, generic request/event metrics, zero workflow histogram
    count/sum, zero rejection count, no rejection warning, and no historical workflow spans.
  - `malformed_workflow_admission_after_authentication_has_no_specialized_effects` uses a valid
    authenticated `owner/repository` identity with `workflow_job: null`. It asserts `204`, one
    durable claim, one generic completed event, no workflow histogram/rejection/warning/span effect.
  - `malformed_detailed_workflow_projection_observes_admission_once_without_rejection` uses a
    structurally admitted completed job with one step missing its required number. It asserts
    `204`, one durable claim, one histogram observation with sum `1.0`, zero rejection count,
    no warning, and no historical workflow spans.
- Existing generic metrics remain explicitly asserted in all new integration branches so generic
  webhook accounting is distinguished from specialized workflow effects.

### Finding 2: valid minimum configuration

- `src/config.rs` now includes `valid_minimum_workflow_job_max_steps_is_accepted`, which loads
  `GHE_WORKFLOW_JOB_MAX_STEPS=1` and asserts the getter returns `1`.
- Existing default, `1024`, `0`, and `1025` coverage remains unchanged.

### Finding 3: obsolete metric keep-alives

- `src/metrics.rs` removes the no-op function-pointer references for
  `observe_workflow_job_steps` and `record_workflow_trace_rejection`.
- The real metric APIs and behavior are unchanged.

### Finding 4: changelog

- Added `changelog/2026-08-07T10-05-15-0400-workflow-job-step-limit-final-review.md` with the
  coverage iteration, privacy boundary, cleanup, and validation context.

## Initial test result and RED/GREEN record

The new tests were added before any production edit and then run directly:

- Minimum configuration test: 1 passed.
- Direct admission rejection filter: 5 passed.
- New unauthorized/non-completed integration test: 1 passed.
- New malformed-admission integration test: 1 passed.
- New detailed-projection-failure integration test: 1 passed.

Total initial focused result: 9 passed, 0 failed. The tests did not expose a production defect;
no false RED assertion was introduced and no production correction was made.

During the first full validation after adding coverage, the pre-existing over-limit privacy test
occasionally failed because it searched the entire dynamic Prometheus exposition for the raw digit
substring `9901`. An unrelated timing or body-size sample can contain those digits. This was a
real test-quality defect, not a production defect. The minimal test-only correction changes that
assertion to compare exact parsed Prometheus sample values, preserving the identifier privacy
check without timing-dependent substring matches. The corrected over-limit test passed in 20
isolated repetitions.

A separate full-suite run also showed two unrelated intermittent OTLP capture misses in existing
repository/webhook span tests. Their lifecycle summaries contained the expected spans while the
flushed capture did not. Ten subsequent complete-suite repetitions passed, followed by a final
required validation run that passed. This remains a test-harness timing concern, not a change
introduced by this wave.

## Final validation

Final required commands, run in order, all exited successfully:

```text
just fmt
cargo build
cargo clippy --all-targets -- -D warnings
just test
cargo doc --no-deps
```

Observed output:

- `just fmt`: `cargo fmt --all -- --check`, exit 0.
- `cargo build`: dev profile finished successfully with no warnings.
- `cargo clippy --all-targets -- -D warnings`: check finished successfully with no warnings.
- `just test`: all targets passed:
  - library: 152 passed, 0 failed;
  - binary: 0 passed, 0 failed;
  - `tests/delivery_storage.rs`: 8 passed, 0 failed;
  - `tests/merge_queue_storage.rs`: 10 passed, 0 failed;
  - `tests/repository_api.rs`: 18 passed, 0 failed;
  - `tests/startup.rs`: 5 passed, 0 failed;
  - `tests/storage.rs`: 8 passed, 0 failed;
  - `tests/webhook_api.rs`: 22 passed, 0 failed;
  - total: 223 passed, 0 failed.
- `cargo doc --no-deps`: generated
  `target/doc/github_webhook_exporter/index.html`, exit 0.
- `git diff --check`: exit 0.

The focused new-test commands were also rerun successfully after formatting. The 20-repeat
isolated over-limit privacy test and 10-repeat complete-suite stability check passed.

## Files changed

- `src/api/workflow_job.rs`
- `src/config.rs`
- `src/metrics.rs`
- `src/telemetry/otlp_test.rs`
- `changelog/2026-08-07T10-05-15-0400-workflow-job-step-limit-final-review.md`
- This report: `.superpowers/sdd/2026-08-07-workflow-job-step-limit/final-fix-report.md`

## Commit

Implementation and review-fix commit:

```text
bc3d794 test: cover workflow job step-limit final review gaps
```

The report is being committed separately so it can include the finalized implementation commit
identity without changing that implementation commit.

## Self-review

- Unauthorized coverage uses a real malformed signature/header authentication path and confirms
  that authentication fails before durable claiming or specialized admission.
- Post-claim malformed cases contain a valid configured repository identity, so authentication and
  claiming are real rather than bypassed fixtures.
- Non-completed over-limit payloads are authenticated but never enter completed-workflow admission.
- Histogram count and sum, rejection counter, warning message, generic event/request metrics,
  durable claims, HTTP status, and historical span absence are asserted at the branch where each
  behavior matters.
- No repository, delivery, run, or job identifier was added as a Prometheus label.
- No production behavior, real metric API, or privacy boundary was changed.
- No push or PR comment was performed.

## Concerns

The existing OTLP integration harness has a low-frequency parallel timing flake where lifecycle
capture can contain a span that the force-flushed OTLP capture misses. It passed 10 consecutive
full-suite repeats and the final required sequence, but should remain visible for future harness
hardening. No other concerns were found.
