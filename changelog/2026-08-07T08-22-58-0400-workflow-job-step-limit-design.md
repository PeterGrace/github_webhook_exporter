# Workflow Job Step Limit Design

- Added the approved design for a configurable completed workflow-job step limit.
- Selected a default of 256 steps and a hard configurable maximum of 1,024.
- Defined bounded pre-projection counting, all-or-nothing trace rejection, a step-count histogram,
  and a bounded rejection counter.
- Documented an actionable `too_many_steps` warning containing validated repository, delivery,
  workflow-run, and job identifiers while continuing to exclude names and payload data.
