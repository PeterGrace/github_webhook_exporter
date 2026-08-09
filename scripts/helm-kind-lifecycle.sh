#!/usr/bin/env bash
set -Eeuo pipefail
umask 077

SCRIPT_DIRECTORY="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIRECTORY
REPOSITORY_ROOT="$(cd -- "${SCRIPT_DIRECTORY}/.." && pwd)"
readonly REPOSITORY_ROOT
# shellcheck source=scripts/helm-kind-lifecycle-lib.sh disable=SC1091
source "${SCRIPT_DIRECTORY}/helm-kind-lifecycle-lib.sh"

if [[ "$#" -ne 3 || -z "$1" || -z "$2" || -z "$3" ]]; then
    printf 'usage: %s CHART_DIRECTORY IMAGE ARTIFACT_DIRECTORY\n' "${0##*/}" >&2
    exit 2
fi
readonly CHART_DIRECTORY="$1"
readonly IMAGE="$2"
readonly ARTIFACT_DIRECTORY="$3"
readonly RELEASE_NAME='github-webhook-exporter'
readonly NAMESPACE='github-webhook-exporter-test'
readonly RESOURCE_NAME='github-webhook-exporter'
readonly POD_NAME='github-webhook-exporter-0'
readonly REPOSITORY_NAME='acceptance/repository'
readonly PULL_REQUEST_ENQUEUED="${REPOSITORY_ROOT}/ci/kind/pull-request-enqueued.json"
readonly PULL_REQUEST_DEQUEUED="${REPOSITORY_ROOT}/ci/kind/pull-request-dequeued.json"
readonly MERGE_GROUP_OPEN="${REPOSITORY_ROOT}/ci/kind/merge-group-checks-requested.json"
readonly MERGE_GROUP_CLOSE="${REPOSITORY_ROOT}/ci/kind/merge-group-destroyed.json"

for command_name in base64 curl docker find grep head helm jq kind kubectl mktemp python3 rm sleep; do
    require_command "${command_name}"
done
if [[ ! -f "${CHART_DIRECTORY}/Chart.yaml" ]]; then
    fail 'invalid Helm chart directory'
    exit 2
fi
for fixture in "${PULL_REQUEST_ENQUEUED}" "${PULL_REQUEST_DEQUEUED}" \
    "${MERGE_GROUP_OPEN}" "${MERGE_GROUP_CLOSE}"; do
    if [[ ! -f "${fixture}" ]]; then
        fail 'required webhook fixture is missing'
        exit 2
    fi
done

TEMPORARY_DIRECTORY="$(mktemp -d)"
readonly TEMPORARY_DIRECTORY
readonly KUBECONFIG_PATH="${TEMPORARY_DIRECTORY}/kubeconfig"
readonly MASTER_KEY_FILE="${TEMPORARY_DIRECTORY}/master-key"
readonly ADMIN_TOKEN_FILE="${TEMPORARY_DIRECTORY}/admin-token"
readonly WEBHOOK_SECRET_FILE="${TEMPORARY_DIRECTORY}/webhook-secret"
readonly REPOSITORY_REQUEST_FILE="${TEMPORARY_DIRECTORY}/repository.json"
readonly HTTP_RESPONSE_FILE="${TEMPORARY_DIRECTORY}/response"
readonly HTTP_CONFIG_FILE="${TEMPORARY_DIRECTORY}/curl.conf"
readonly PORT_FORWARD_LOG="${TEMPORARY_DIRECTORY}/port-forward.log"
readonly STATUS_FILE="${ARTIFACT_DIRECTORY}/http-statuses.txt"
CLUSTER_SUFFIX="$(head -c 9 /dev/urandom | base64 | tr '/+' 'ab' | tr -d '=')"
CLUSTER_SUFFIX="${CLUSTER_SUFFIX,,}"
readonly CLUSTER_NAME="gwe-lifecycle-${CLUSTER_SUFFIX}"
readonly KUBE_CONTEXT="kind-${CLUSTER_NAME}"
CLUSTER_CREATED=false
PORT_FORWARD_PID=''
LOCAL_PORT=''

kube() {
    kubectl --kubeconfig "${KUBECONFIG_PATH}" --context "${KUBE_CONTEXT}" "$@"
}

capture_diagnostics() {
    [[ "${CLUSTER_CREATED}" == true ]] || return 0
    mkdir -p "${ARTIFACT_DIRECTORY}"
    helm get manifest "${RELEASE_NAME}" --namespace "${NAMESPACE}" \
        --kubeconfig "${KUBECONFIG_PATH}" --kube-context "${KUBE_CONTEXT}" \
        >"${ARTIFACT_DIRECTORY}/objects.yaml" 2>/dev/null || true
    kube --namespace "${NAMESPACE}" get pods -o wide \
        >"${ARTIFACT_DIRECTORY}/pods.txt" 2>&1 || true
    kube --namespace "${NAMESPACE}" describe statefulset "${RESOURCE_NAME}" \
        >"${ARTIFACT_DIRECTORY}/statefulset.txt" 2>&1 || true
    kube --namespace "${NAMESPACE}" get events --sort-by=.metadata.creationTimestamp \
        >"${ARTIFACT_DIRECTORY}/events.txt" 2>&1 || true
    kube --namespace "${NAMESPACE}" logs "${POD_NAME}" \
        >"${ARTIFACT_DIRECTORY}/logs-current.txt" 2>&1 || true
    kube --namespace "${NAMESPACE}" logs "${POD_NAME}" --previous \
        >"${ARTIFACT_DIRECTORY}/logs-previous.txt" 2>&1 || true
}

stop_port_forward() {
    if [[ -n "${PORT_FORWARD_PID}" ]]; then
        kill "${PORT_FORWARD_PID}" >/dev/null 2>&1 || true
        wait "${PORT_FORWARD_PID}" >/dev/null 2>&1 || true
        PORT_FORWARD_PID=''
    fi
}

cleanup() {
    local original_status=$?
    local cleanup_status=0
    trap - EXIT
    stop_port_forward
    capture_diagnostics
    if [[ -d "${ARTIFACT_DIRECTORY}" && -s "${MASTER_KEY_FILE}" && \
        -s "${ADMIN_TOKEN_FILE}" && -s "${WEBHOOK_SECRET_FILE}" ]]; then
        if ! scan_private_artifacts "${ARTIFACT_DIRECTORY}" \
            "${MASTER_KEY_FILE}" "${ADMIN_TOKEN_FILE}" "${WEBHOOK_SECRET_FILE}"; then
            cleanup_status=1
        fi
    fi
    if [[ "${CLUSTER_CREATED}" == true && "${KEEP_KIND_CLUSTER:-false}" != true ]]; then
        kind delete cluster --name "${CLUSTER_NAME}" \
            --kubeconfig "${KUBECONFIG_PATH}" >/dev/null 2>&1 || cleanup_status=1
    elif [[ "${CLUSTER_CREATED}" == true ]]; then
        printf 'Preserved Kind cluster %s with kubeconfig %s\n' \
            "${CLUSTER_NAME}" "${KUBECONFIG_PATH}" >&2
    fi
    if [[ "${KEEP_KIND_CLUSTER:-false}" != true ]]; then
        rm -rf "${TEMPORARY_DIRECTORY}" || cleanup_status=1
    fi
    if [[ "${original_status}" -ne 0 ]]; then
        exit "${original_status}"
    fi
    exit "${cleanup_status}"
}
trap cleanup EXIT

wait_for_pod_ready() {
    if ! kube --namespace "${NAMESPACE}" wait --for=condition=Ready \
        "pod/${POD_NAME}" --timeout=180s >/dev/null; then
        fail 'application pod did not become Ready'
        return 1
    fi
}

start_port_forward() {
    local deadline=$((SECONDS + 30))
    stop_port_forward
    : >"${PORT_FORWARD_LOG}"
    kube --namespace "${NAMESPACE}" port-forward "service/${RESOURCE_NAME}" ':8080' \
        >"${PORT_FORWARD_LOG}" 2>&1 &
    PORT_FORWARD_PID=$!
    while (( SECONDS < deadline )); do
        LOCAL_PORT="$(grep -Eo '127\.0\.0\.1:[0-9]+' "${PORT_FORWARD_LOG}" | head -n 1 | cut -d: -f2 || true)"
        if [[ "${LOCAL_PORT}" =~ ^[0-9]+$ ]]; then
            return 0
        fi
        if ! kill -0 "${PORT_FORWARD_PID}" >/dev/null 2>&1; then
            break
        fi
        sleep 1
    done
    fail 'service port-forward did not become available'
}

write_curl_config() {
    local method="$1"
    local url="$2"
    local output_file="$3"
    {
        printf 'silent\nshow-error\nrequest = "%s"\nurl = "%s"\noutput = "%s"\n' \
            "${method}" "${url}" "${output_file}"
        printf 'write-out = "%%{http_code}"\n'
    } >"${HTTP_CONFIG_FILE}"
}

request_status() {
    local method="$1"
    local path="$2"
    local label="$3"
    local expected="$4"
    local status
    write_curl_config "${method}" "http://127.0.0.1:${LOCAL_PORT}${path}" \
        "${HTTP_RESPONSE_FILE}"
    status="$(curl --config "${HTTP_CONFIG_FILE}")"
    record_http_status "${STATUS_FILE}" "${label}" "${status}"
    assert_equal "${expected}" "${status}" "${label} HTTP status"
}

admin_request() {
    local method="$1"
    local path="$2"
    local body_file="$3"
    local label="$4"
    local expected="$5"
    local status
    write_curl_config "${method}" "http://127.0.0.1:${LOCAL_PORT}${path}" \
        "${HTTP_RESPONSE_FILE}"
    {
        printf 'header = "Authorization: Bearer %s"\n' "$(<"${ADMIN_TOKEN_FILE}")"
        printf 'header = "Content-Type: application/json"\n'
        if [[ -n "${body_file}" ]]; then
            printf 'data-binary = "@%s"\n' "${body_file}"
        fi
    } >>"${HTTP_CONFIG_FILE}"
    status="$(curl --config "${HTTP_CONFIG_FILE}")"
    record_http_status "${STATUS_FILE}" "${label}" "${status}"
    assert_equal "${expected}" "${status}" "${label} HTTP status"
}

send_webhook() {
    local event_type="$1"
    local delivery_id="$2"
    local payload_file="$3"
    local label="$4"
    local status
    local signature
    signature="$(hmac_sha256 "${WEBHOOK_SECRET_FILE}" "${payload_file}")"
    write_curl_config POST "http://127.0.0.1:${LOCAL_PORT}/webhooks/github" \
        "${HTTP_RESPONSE_FILE}"
    {
        printf 'header = "Content-Type: application/json"\n'
        printf 'header = "X-GitHub-Event: %s"\n' "${event_type}"
        printf 'header = "X-GitHub-Delivery: %s"\n' "${delivery_id}"
        printf 'header = "X-Hub-Signature-256: %s"\n' "${signature}"
        printf 'data-binary = "@%s"\n' "${payload_file}"
    } >>"${HTTP_CONFIG_FILE}"
    signature=''
    status="$(curl --config "${HTTP_CONFIG_FILE}")"
    : >"${HTTP_CONFIG_FILE}"
    record_http_status "${STATUS_FILE}" "${label}" "${status}"
    assert_equal 204 "${status}" "${label} HTTP status"
}

fetch_metrics() {
    write_curl_config GET "http://127.0.0.1:${LOCAL_PORT}/metrics" \
        "${TEMPORARY_DIRECTORY}/metrics"
    local status
    status="$(curl --config "${HTTP_CONFIG_FILE}")"
    assert_equal 200 "${status}" 'metrics HTTP status'
}

assert_metric() {
    local expected="$1"
    fetch_metrics
    if ! grep --fixed-strings --quiet "${expected}" "${TEMPORARY_DIRECTORY}/metrics"; then
        fail "expected bounded metric is absent: ${expected}"
        return 1
    fi
}

rm -rf "${ARTIFACT_DIRECTORY}"
mkdir -p "${ARTIFACT_DIRECTORY}"
: >"${STATUS_FILE}"
head -c 32 /dev/urandom | base64 | tr -d '\n' >"${MASTER_KEY_FILE}"
head -c 24 /dev/urandom | base64 | tr -d '\n' >"${ADMIN_TOKEN_FILE}"
head -c 24 /dev/urandom | base64 | tr -d '\n' >"${WEBHOOK_SECRET_FILE}"
chmod 600 "${MASTER_KEY_FILE}" "${ADMIN_TOKEN_FILE}" "${WEBHOOK_SECRET_FILE}"

CLUSTER_CREATED=true
kind create cluster --name "${CLUSTER_NAME}" --kubeconfig "${KUBECONFIG_PATH}" --wait 90s
kind load docker-image "${IMAGE}" --name "${CLUSTER_NAME}"
kube create namespace "${NAMESPACE}"
kube --namespace "${NAMESPACE}" create secret generic "${RESOURCE_NAME}" \
    --from-file="master-key=${MASTER_KEY_FILE}" \
    --from-file="admin-token=${ADMIN_TOKEN_FILE}"

image_repository="${IMAGE%:*}"
image_tag="${IMAGE##*:}"
helm install "${RELEASE_NAME}" "${CHART_DIRECTORY}" \
    --namespace "${NAMESPACE}" --kubeconfig "${KUBECONFIG_PATH}" \
    --kube-context "${KUBE_CONTEXT}" --atomic --wait --timeout 3m \
    --set-string "image.repository=${image_repository}" \
    --set-string "image.tag=${image_tag}" --set image.pullPolicy=Never \
    --set application.shutdownTimeoutSeconds=5 \
    --set telemetry.shutdownTimeoutSeconds=2 \
    --set telemetry.timeoutMilliseconds=100 \
    --set-string telemetry.endpoint=http://127.0.0.1:9 \
    --set terminationGracePeriodSeconds=10

wait_for_pod_ready
start_port_forward
request_status GET /health/live live_initial 200
request_status GET /health/ready ready_initial 200
request_status GET /metrics metrics_initial 200

jq --null-input --arg full_name "${REPOSITORY_NAME}" \
    --rawfile webhook_secret "${WEBHOOK_SECRET_FILE}" \
    '{full_name: $full_name, webhook_secret: $webhook_secret, enabled: true}' \
    >"${REPOSITORY_REQUEST_FILE}"
admin_request POST /api/v1/repositories "${REPOSITORY_REQUEST_FILE}" repository_create 201
send_webhook pull_request 550e8400-e29b-41d4-a716-446655440101 \
    "${PULL_REQUEST_ENQUEUED}" pull_request_enqueue
send_webhook merge_group 550e8400-e29b-41d4-a716-446655440102 \
    "${MERGE_GROUP_OPEN}" merge_group_open
assert_metric 'github_repository_configurations 1'
assert_metric 'github_webhook_events_total{event_type="pull_request",action="enqueued"} 1'
assert_metric 'github_webhook_events_total{event_type="merge_group",action="checks_requested"} 1'

old_uid="$(kube --namespace "${NAMESPACE}" get pod "${POD_NAME}" -o jsonpath='{.metadata.uid}')"
kube --namespace "${NAMESPACE}" delete pod "${POD_NAME}" --wait=true >/dev/null
wait_for_pod_ready
new_uid="$(kube --namespace "${NAMESPACE}" get pod "${POD_NAME}" -o jsonpath='{.metadata.uid}')"
if [[ "${new_uid}" == "${old_uid}" ]]; then
    fail 'pod restart did not replace the pod UID'
    exit 1
fi
start_port_forward
admin_request GET /api/v1/repositories '' repository_list_after_restart 200
if ! jq --exit-status --arg name "${REPOSITORY_NAME}" \
    'length == 1 and .[0].full_name == $name and .[0].enabled == true' \
    "${HTTP_RESPONSE_FILE}" >/dev/null; then
    fail 'repository configuration did not survive pod restart'
    exit 1
fi
send_webhook pull_request 550e8400-e29b-41d4-a716-446655440101 \
    "${PULL_REQUEST_ENQUEUED}" duplicate_after_restart
send_webhook pull_request 550e8400-e29b-41d4-a716-446655440103 \
    "${PULL_REQUEST_DEQUEUED}" pull_request_dequeue_after_restart
send_webhook merge_group 550e8400-e29b-41d4-a716-446655440104 \
    "${MERGE_GROUP_CLOSE}" merge_group_close_after_restart
assert_metric 'github_webhook_duplicates_total 1'
assert_metric 'github_merge_queue_pr_outcomes_total{outcome="unknown",reason="unclassified_dequeue"} 1'
assert_metric 'github_merge_queue_attempt_duration_seconds_count{outcome="unknown"} 1'
assert_metric 'github_webhook_events_total{event_type="merge_group",action="destroyed"} 1'

collector_failure_seen=false
for ((attempt = 0; attempt < 30; attempt++)); do
    fetch_metrics
    if grep --extended-regexp --quiet \
        'github_telemetry_export_failures_total\{signal="(trace|log)",reason="(transport|timeout)"\} [1-9]' \
        "${TEMPORARY_DIRECTORY}/metrics"; then
        collector_failure_seen=true
        break
    fi
    sleep 1
done
if [[ "${collector_failure_seen}" != true ]]; then
    fail 'unavailable collector did not produce a normalized failure metric'
    exit 1
fi
request_status GET /health/live live_during_collector_outage 200
request_status GET /health/ready ready_during_collector_outage 200
send_webhook merge_group 550e8400-e29b-41d4-a716-446655440105 \
    "${MERGE_GROUP_OPEN}" webhook_during_collector_outage

capture_diagnostics
scan_private_artifacts "${ARTIFACT_DIRECTORY}" \
    "${MASTER_KEY_FILE}" "${ADMIN_TOKEN_FILE}" "${WEBHOOK_SECRET_FILE}"
printf 'Kind lifecycle acceptance passed; diagnostics: %s\n' "${ARTIFACT_DIRECTORY}"
