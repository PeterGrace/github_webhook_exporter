# Local Cluster Lifecycle Acceptance Design

## Goal

Prove the packaged production image and Helm chart satisfy the application's lifecycle,
persistence, privacy, observability-isolation, and singleton SQLite-writer contracts in a real,
disposable Kubernetes cluster.

## Scope

The acceptance suite will build and load the production `linux/amd64` image into a fresh Kind
cluster, create test credentials at runtime, install the production chart, exercise the running
application through Kubernetes networking, and destroy every resource on success or failure.
It covers issue #47 only and does not add backup/restore, high availability, cloud-provider storage,
ingress, certificate, or load testing.

## Architecture

The existing Bash and Kind testing surface will be extended rather than introducing a Python or
Rust orchestration dependency. A lifecycle orchestrator will own cluster setup, image loading,
namespace and Secret creation, Helm installation, application assertions, diagnostic collection,
credential scanning, and cleanup. Focused shell helpers will keep command execution, HTTP requests,
webhook signing, polling, and assertions independently understandable.

A static shell contract test will define the required lifecycle stages before the orchestrator is
changed. It will verify that the harness invokes image loading, readiness checks, API and webhook
flows, restart and rollout checks, broken-storage validation, diagnostics, redaction scanning, and
cleanup. Real Kind execution remains the authoritative behavioral test.

The `justfile` will expose one reproducible lifecycle recipe. GitHub Actions will invoke it and
upload its diagnostics with `if: always()` so failures preserve useful evidence.

## Cluster and Secret Lifecycle

Each run will use a collision-resistant cluster name, private temporary directory, dedicated
kubeconfig, namespace, release, and artifact directory. The harness will probe all required tools
before creating resources. An EXIT trap will collect final diagnostics when appropriate, uninstall
the release, delete the owned Kind cluster, and remove credential-bearing temporary files.

The master key, administrator token, and repository webhook secret will be generated at runtime
with restrictive file permissions. Kubernetes Secrets will be created from files, not literal
command-line values. Secret variables will never be traced, printed, embedded in manifests, or
included in assertion messages.

## Runtime Acceptance Flow

1. Build the production image, create the Kind cluster, load the image, and install the chart with
   persistence, short bounded lifecycle deadlines, and an unavailable OTLP endpoint.
2. Wait for the StatefulSet and verify `/health/live`, `/health/ready`, and `/metrics` through a
   controlled port-forward.
3. Create a repository through the authenticated administration API and submit correctly signed
   pull-request and merge-group webhook transitions with unique delivery identifiers.
4. Assert bounded Prometheus families and statuses, then delete the pod and wait for its replacement.
5. Confirm repository configuration remains usable, a duplicate delivery remains claimed, and a
   pre-restart merge-queue attempt can be completed after restart.
6. Confirm collector failure diagnostics remain normalized while probes and authenticated webhook
   acceptance remain healthy.
7. Create an isolated broken-readiness case by overriding the database path to a location the
   non-root, read-only container cannot write; verify the pod never reaches Ready and its health
   endpoint cannot produce a false successful rollout.
8. During controlled webhook activity, send SIGTERM by deleting the application pod and verify the
   process exits and replacement becomes Ready within the configured termination grace period,
   without a `preStop` hook.
9. Trigger a chart/config rollout and sample running pod/container identities that reference the
   StatefulSet PVC. Record and assert an observed maximum of one active exporter container with
   that volume; this bounds observed Kubernetes status rather than claiming sub-sample overlap is
   impossible.

## Diagnostics and Privacy

The harness will capture rendered release objects, redacted HTTP status/assertion records, pod and
StatefulSet descriptions, namespace events, and current/previous container logs. Diagnostic
collection must tolerate partial cluster failure and must not mask the original test status.

Before reporting success, the harness will recursively scan captured artifacts for all generated
credential values and forbidden webhook payload material. A match fails the suite with only the
artifact path and category, never the matched value. CI uploads the artifact directory on both
success and failure; local runs print its location.

## Error Handling

All shell scripts use strict mode and restrictive umasks. Expected negative tests capture command
status and sanitized output explicitly. Polling uses fixed deadlines and reports the failed
contract rather than unbounded retries. Cleanup preserves the original failure status unless the
test succeeded and cleanup itself failed.

## Validation

Implementation is complete only when these gates pass:

- The new static harness contract test first fails against the old harness and then passes.
- The lifecycle recipe passes against a disposable Kind cluster.
- Generated diagnostic artifacts contain no test credentials or forbidden payload material.
- `just fmt`
- `cargo build`
- `cargo clippy --all-targets -- -D warnings`
- `just test`
- `cargo doc --no-deps`
- ShellCheck for all tracked shell scripts.

## Alternatives Rejected

- A Python orchestrator would improve structured scripting but add a runtime and testing surface
  solely for process orchestration already established in Bash.
- A Rust driver would provide stronger types but add disproportionate dependencies and compile cost
  while still shelling out to Docker, Helm, Kind, and kubectl.
