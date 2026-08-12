# Address PR #80 review

- Clarified that trace, log, and Sentry shutdown workers start concurrently under one shared deadline.
- Confirmed the workflow test helper no longer contains the reviewed let-and-return pattern.
- Documented that Sentry SDK internal queue-overflow drops are not exposed through its capture API and that canonical OTLP exception events remain the observable source of record.
