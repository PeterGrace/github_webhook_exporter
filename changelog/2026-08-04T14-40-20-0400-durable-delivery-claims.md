# Durable webhook delivery claims and retention

- Added an embedded `processed_deliveries` migration containing only a delivery UUID and receipt timestamp, plus an indexed retention timestamp.
- Added a UUID-backed, debug-redacted `DeliveryId` value type that rejects malformed identifiers and normalizes persisted text.
- Added a focused `DeliveryStore` whose single-statement claim reports new or duplicate deliveries without replacing the original receipt time.
- Added a bounded prune operation that deletes no more than 1,000 expired claims per call so lifecycle code can cancel between batches.
- Normalized busy and locked SQLite failures as unavailable and discarded unexpected SQLite details from public delivery-store errors.
- Added migration, durability, duplicate, concurrency, redaction, lock-contention, and retention integration coverage.

A crash after a claim commits but before its metric increment can undercount one delivery. This storage contract prevents duplicate counting during uninterrupted operation but does not promise exactly-once metrics across crashes.
