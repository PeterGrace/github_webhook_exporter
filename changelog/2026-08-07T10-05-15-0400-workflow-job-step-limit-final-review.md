# Workflow Job Step Limit Final Review Coverage

This final review iteration strengthens regression coverage for the bounded workflow-job trace
admission path.

- Added direct admission tests for malformed JSON bytes, missing, null, and wrong-type
  `workflow_job` envelopes.
- Added valid minimum configuration coverage for `GHE_WORKFLOW_JOB_MAX_STEPS=1`, while retaining
  the existing zero, 1,024, and 1,025 boundary coverage.
- Added authenticated and unauthorized integration coverage for specialized exclusions, malformed
  admission, and detailed projection failure. The tests verify HTTP status, durable delivery
  claims, generic webhook metrics, workflow step histograms, rejection counters, warning output,
  and historical span absence.
- Removed obsolete function-pointer keep-alives for the now-used workflow metric methods.
- Tightened the existing numeric privacy assertion to compare exact Prometheus sample values,
  avoiding false failures when unrelated dynamic timing samples contain an identifier's digits.

The rejection diagnostic privacy boundary remains unchanged: only the approved repository,
delivery, workflow identifiers, observed count, configured limit, and fixed rejection reason may
appear in the bounded warning. Workflow names, step data, payload fragments, secrets, headers,
URLs, and collector details remain excluded.
