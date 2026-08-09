# Issue 46 blocking fix round 1

## Changed

- Made successful `renameat2` exchange/no-replace operations the irreversible Helm output commit
  point and moved old-output deletion into a separate identity-bound cleanup operation.
- Added a stable, value-free deferred-cleanup warning while preserving successful commit status and
  retryable shell state when old-output deletion fails.
- Made commit retry recognize the exact already-exchanged inode pair after interruption, while all
  unsupported or pre-commit rename failures continue to fail closed without changing either inode.
- Combined generated-stage creation, ownership marking, and snapshot validation in the locked
  Python helper; malformed helper output now triggers cleanup using separately returned parent and
  stage identities.
- Expanded deterministic fault coverage for cleanup failure, exchange interruption, unsupported
  `renameat2`, malformed preparation output, parent/destination substitution, decoy preservation,
  repeated `/tmp` output, and repository `dist` plus `dist/rendered` replacement.
- Required normalized archive-collision extraction attempts to return exact `ARCHIVE004` while
  leaving a sentinel destination untouched.

## Validation

- `python3 scripts/helm-output-directory-test.py`
- `python3 scripts/helm-archive-preflight-test.py`
- `just helm-security-test`
- `just workflow-test`
- `just helm-static`
- Full Rust and repository gates documented in the blocking-fixes report.
