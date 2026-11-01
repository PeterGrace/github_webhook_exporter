# Reject dot and empty branch path segments

Date: 2026-11-01T01:50:02Z

## What changed

- `WorkflowBranch::sanitize` now returns `None` for a branch name with an empty, `.`, or `..`
  path segment. That covers a leading, trailing, or doubled `/` too. The check runs after
  control-character removal and truncation, on the exact text that reaches the GitHub API URL.
- The `BRANCH_SEGMENT_ENCODE_SET` doc comment in `src/github/client.rs` now names this check,
  not Git's ref-name rules, as what stops path traversal. `/` stays literal, so multi-segment
  branches such as `gh-readonly-queue/main/pr-7` still resolve.
- Added unit tests for accepted dotted names, rejected `..`/`.`/empty segments (including
  `main/../../../../orgs/x`), and a segment left empty by truncation.

## Why

Branch names from signed webhook payloads went into the branch-protection request path with `/`
and `.` unencoded. A name like `main/../../../../orgs/x` was normalized by the `url` crate into a
different API path, and that request carried the installation token. Git refuses such ref names,
so rejecting them drops no real branch. A rejected name means the run gets no branch metadata and
no required-check lookup, the same as when the payload has no branch at all.

## Alternatives considered

Percent-encoding `/` and `.` was rejected: GitHub resolves multi-segment branch names only when
the separators stay unencoded (see the existing `a_multi_segment_branch_keeps_its_separators_unencoded`
test).

Commit-pinned implementation and test evidence (EVD-0290, EVD-0291) is in the claims ledger.
CLM-0037 is marked supported, and the merge-queue (CLM-0004) and root (CLM-0001) rollups are
refreshed. Both stay refuted because of other leaves.

Known edge: a `workflow_run_contexts` row stored before this change with a dot-segment branch
now fails `validated_stored_branch` on load. A job webhook for that run then answers 503 until
retention prunes the row. Only a payload signed with the webhook secret could have stored one.

This resolves the CLM-0037 security finding tracked by issue #112.
