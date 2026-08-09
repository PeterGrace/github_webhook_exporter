# Version-tag image and chart publication design

- Defined a tag-only GHCR release architecture for the production image and validated Helm archive.
- Specified least-privilege permissions, pre-authentication validation, immutable artifact handling,
  and a guarded chart-only recovery path based on exact image digest equality.
- Documented the release state machine, test boundaries, operator documentation requirements, and
  non-atomic cross-artifact failure behavior for issue #54.
