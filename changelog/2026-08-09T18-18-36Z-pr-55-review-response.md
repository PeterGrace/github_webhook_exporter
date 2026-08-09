# PR 55 review response

- Linked exact graceful-shutdown log assertions to their `src/main.rs` source.
- Timed old-pod termination independently from replacement readiness so slow startup cannot create
  a false SIGTERM deadline failure.
- Clarified that rollout sampling bounds observed Kubernetes status and cannot prove overlap shorter
  than the sample interval is impossible.
- Aligned the committed implementation plan with Helm 4 `--rollback-on-failure` behavior.
