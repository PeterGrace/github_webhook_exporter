# Version-independent validated chart artifact

- Updated workflow validation to require a wildcard Helm archive artifact path.
- Derived the packaged archive filename from embedded chart metadata instead of hard-coding `0.1.0`.
- Preserved archive render, schema, policy, and secret validation while keeping the current release archive verifiable.
- Confirmed the wildcard GitHub Actions artifact upload still uses 30-day retention.
