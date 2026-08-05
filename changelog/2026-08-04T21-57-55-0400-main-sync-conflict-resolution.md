# Main branch synchronization

- Merged the latest `origin/main` into the bounded Prometheus metrics branch.
- Integrated the metrics component with the current repository store, administrator authentication, health routing, and graceful-shutdown application state.
- Regenerated the dependency lockfile from the resolved dependency union and retained the unauthenticated `/metrics` route coverage.
