# Specialized merge-group webhook metrics

- Dispatch authenticated, newly claimed `merge_group.checks_requested` and
  `merge_group.destroyed` deliveries to bounded specialized metric updates.
- Normalize missing, non-string, mixed-case, unknown, and malicious destroyed reasons to `other`
  without retaining attacker-controlled values.
- Preserve generic event metrics, duplicate suppression, `204 No Content` responses, and complete
  isolation from durable pull-request merge-queue attempts.
- Add signed router coverage for supported reasons, unsupported actions, duplicates, state
  isolation, and sensitive-output redaction.
- Document authoritative group-level merge statistics and their separation from per-PR outcomes.
