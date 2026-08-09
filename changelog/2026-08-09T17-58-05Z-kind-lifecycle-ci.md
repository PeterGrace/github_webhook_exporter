# Kind lifecycle CI integration

- Pinned checksum-verified Kind 0.31.0 and kubectl 1.35.0 in the existing CI tool installer.
- Added the real cluster lifecycle gate to GitHub Actions and retained redacted diagnostics for 14
  days with an always-run upload step.
- Documented local execution, covered contracts, diagnostic privacy, normal cleanup, and the
  explicit preserve-for-debugging escape hatch.
