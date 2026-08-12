# PR 74 review response

- Changed durable workflow branch reconstruction to reapply current sanitization without requiring
  byte-for-byte equality with the stored value.
- This preserves bounded, control-free OTLP attributes while allowing records written under an
  earlier Unicode classification to degrade safely instead of making completed-job webhook
  processing return `503 Service Unavailable`.
- Added a regression test covering a legacy stored branch containing a newly disallowed control
  character.
- Confirmed that SQLite lookup failures remain deliberately fail-closed, workflow-run transitions
  deliberately refresh correlation retention, and existing projection coverage already proves an
  authoritative `head_branch` overrides a conflicting pull-request head ref.
