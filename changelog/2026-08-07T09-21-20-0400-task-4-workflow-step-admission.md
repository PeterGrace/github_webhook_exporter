# Task 4: Workflow-job step admission diagnostics

Implemented all-or-nothing admission for completed workflow-job traces. Structurally valid
jobs now record their exact step count, accept traces at the configured inclusive limit, and
reject over-limit traces before detailed projection. Rejections remain claimed and return HTTP
204, increment the bounded `too_many_steps` metric once, and emit one parentless warning with
only the approved repository, delivery, workflow identifiers, step count, and limit fields.

Added OTLP/stderr privacy and duplicate-delivery coverage for exact-limit acceptance and
over-limit rejection. Removed temporary admission API lint keep-alives now that the handler is
the production caller.
