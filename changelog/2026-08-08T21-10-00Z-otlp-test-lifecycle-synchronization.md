# OTLP test lifecycle synchronization

- Root-caused intermittent OTLP assertions to fixture captures racing span closure across traced HTTP and SQLite resources.
- Added condition-based span lifecycle signaling and bounded flush passes instead of timing assumptions.
- Made final repository, SQLite, retention, and selected webhook captures consume and tear down traced fixture resources before export assertions.
- Stress-validated the prior repository failure path 100 times and the complete Rust library suite 20 times.
