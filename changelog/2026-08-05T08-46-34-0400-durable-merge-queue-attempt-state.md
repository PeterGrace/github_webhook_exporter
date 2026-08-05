# Durable merge-queue attempt state

- Added the schema-exact `merge_queue_attempts` migration with outcome/completion consistency,
  one-active-attempt enforcement, completed-at indexing, and cascading repository deletion.
- Added validated positive pull-request numbers, canonical UTC event timestamps, bounded outcomes
  and reasons, and completion constructors that restrict Phase 3 to merged successes and
  unclassified dequeues.
- Added transactional, idempotent enqueue and completion operations with typed replay and missing
  attempt results, redacted persistence errors, and bounded completed-attempt pruning preparation.
- Added integration coverage for migration constraints, sequential and concurrent transitions,
  restart durability, rollback, locked/internal errors, cascade deletion, pruning, and forbidden
  persisted fields.
