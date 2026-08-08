#!/usr/bin/env bash
set -Eeuo pipefail

readonly RELEASE_NAME="github-webhook-exporter"
readonly NAMESPACE="github-webhook-exporter-test"
readonly RESOURCE_NAME="github-webhook-exporter"
readonly PVC_NAME="data-github-webhook-exporter-0"

fail() {
    printf 'Helm Kind acceptance failed: %s\n' "$1" >&2
    exit 1
}

if [[ "$#" -ne 1 || -z "$1" ]]; then
    printf 'usage: %s CHART_DIRECTORY\n' "${0##*/}" >&2
    exit 2
fi
readonly CHART_DIRECTORY="$1"

if [[ ! -d "${CHART_DIRECTORY}" || ! -f "${CHART_DIRECTORY}/Chart.yaml" ]]; then
    printf 'invalid Helm chart directory: %s\n' "${CHART_DIRECTORY}" >&2
    exit 2
fi

for command_name in base64 head helm kind kubectl mktemp rm sleep; do
    if ! command -v "${command_name}" >/dev/null 2>&1; then
        printf 'required command not found: %s\n' "${command_name}" >&2
        exit 2
    fi
done

cluster_suffix="$(head -c 12 /dev/urandom | base64)"
cluster_suffix="${cluster_suffix,,}"
cluster_suffix="${cluster_suffix//+/a}"
cluster_suffix="${cluster_suffix//\//b}"
cluster_suffix="${cluster_suffix//=}"
readonly CLUSTER_NAME="gwe-acceptance-${cluster_suffix}"
readonly KUBE_CONTEXT="kind-${CLUSTER_NAME}"

while IFS= read -r existing_cluster; do
    if [[ "${existing_cluster}" == "${CLUSTER_NAME}" ]]; then
        fail "generated cluster name already exists"
    fi
done < <(kind get clusters)

TEMPORARY_DIRECTORY="$(mktemp -d)"
readonly TEMPORARY_DIRECTORY
readonly KUBECONFIG_PATH="${TEMPORARY_DIRECTORY}/kubeconfig"
CLUSTER_OWNED=false
RELEASE_MAY_EXIST=false

cleanup() {
    local original_status=$?
    local cleanup_status=0

    trap - EXIT
    if [[ "${CLUSTER_OWNED}" == true ]]; then
        if [[ "${RELEASE_MAY_EXIST}" == true ]]; then
            helm uninstall "${RELEASE_NAME}" \
                --namespace "${NAMESPACE}" \
                --kubeconfig "${KUBECONFIG_PATH}" \
                --kube-context "${KUBE_CONTEXT}" \
                --ignore-not-found >/dev/null 2>&1 || true
        fi
        if ! kind delete cluster \
            --name "${CLUSTER_NAME}" \
            --kubeconfig "${KUBECONFIG_PATH}"; then
            printf 'failed to delete Kind cluster: %s\n' "${CLUSTER_NAME}" >&2
            cleanup_status=1
        fi
    fi
    if ! rm -rf "${TEMPORARY_DIRECTORY}"; then
        printf 'failed to remove temporary directory\n' >&2
        cleanup_status=1
    fi

    if [[ "${original_status}" -ne 0 ]]; then
        exit "${original_status}"
    fi
    exit "${cleanup_status}"
}
trap cleanup EXIT

# The random name was confirmed absent above, so any partial cluster with this name belongs here.
CLUSTER_OWNED=true
kind create cluster \
    --name "${CLUSTER_NAME}" \
    --kubeconfig "${KUBECONFIG_PATH}" \
    --wait 60s

kubectl --kubeconfig "${KUBECONFIG_PATH}" --context "${KUBE_CONTEXT}" \
    create namespace "${NAMESPACE}"

master_key="$(head -c 32 /dev/urandom | base64)"
admin_token="$(head -c 32 /dev/urandom | base64)"
kubectl --kubeconfig "${KUBECONFIG_PATH}" --context "${KUBE_CONTEXT}" \
    --namespace "${NAMESPACE}" create secret generic "${RESOURCE_NAME}" \
    --from-literal="master-key=${master_key}" \
    --from-literal="admin-token=${admin_token}"
master_key=''
admin_token=''

RELEASE_MAY_EXIST=true
helm install "${RELEASE_NAME}" "${CHART_DIRECTORY}" \
    --namespace "${NAMESPACE}" \
    --kubeconfig "${KUBECONFIG_PATH}" \
    --kube-context "${KUBE_CONTEXT}" \
    --wait=false

# kubectl currently reports StatefulSet pausing as unsupported. Invoking it immediately still
# guards future Kubernetes clients that add support; --wait=false keeps image pulls out of scope.
pause_error=''
if ! pause_error="$(kubectl --kubeconfig "${KUBECONFIG_PATH}" \
    --context "${KUBE_CONTEXT}" --namespace "${NAMESPACE}" \
    rollout pause "statefulset/${RESOURCE_NAME}" 2>&1)"; then
    if [[ "${pause_error}" != *'pausing is not supported'* && \
        "${pause_error}" != *'does not support pausing'* ]]; then
        fail "could not pause StatefulSet rollout: ${pause_error}"
    fi
fi

kubectl --kubeconfig "${KUBECONFIG_PATH}" --context "${KUBE_CONTEXT}" \
    --namespace "${NAMESPACE}" get statefulset "${RESOURCE_NAME}" >/dev/null
kubectl --kubeconfig "${KUBECONFIG_PATH}" --context "${KUBE_CONTEXT}" \
    --namespace "${NAMESPACE}" get service "${RESOURCE_NAME}" >/dev/null
kubectl --kubeconfig "${KUBECONFIG_PATH}" --context "${KUBE_CONTEXT}" \
    --namespace "${NAMESPACE}" get configmap "${RESOURCE_NAME}" >/dev/null

pvc_found=false
for ((attempt = 0; attempt < 60; attempt++)); do
    if kubectl --kubeconfig "${KUBECONFIG_PATH}" --context "${KUBE_CONTEXT}" \
        --namespace "${NAMESPACE}" get pvc "${PVC_NAME}" >/dev/null 2>&1; then
        pvc_found=true
        break
    fi
    sleep 1
done
if [[ "${pvc_found}" != true ]]; then
    fail "PVC ${PVC_NAME} was not created"
fi

replicas="$(kubectl --kubeconfig "${KUBECONFIG_PATH}" --context "${KUBE_CONTEXT}" \
    --namespace "${NAMESPACE}" get statefulset "${RESOURCE_NAME}" \
    --output=jsonpath='{.spec.replicas}')"
if [[ "${replicas}" != 1 ]]; then
    fail "StatefulSet replicas must equal one, found ${replicas}"
fi

run_as_user="$(kubectl --kubeconfig "${KUBECONFIG_PATH}" --context "${KUBE_CONTEXT}" \
    --namespace "${NAMESPACE}" get statefulset "${RESOURCE_NAME}" \
    --output=jsonpath='{.spec.template.spec.securityContext.runAsUser}')"
if [[ "${run_as_user}" != 65532 ]]; then
    fail "StatefulSet pod security UID must equal 65532, found ${run_as_user}"
fi

helm uninstall "${RELEASE_NAME}" \
    --namespace "${NAMESPACE}" \
    --kubeconfig "${KUBECONFIG_PATH}" \
    --kube-context "${KUBE_CONTEXT}"
RELEASE_MAY_EXIST=false

if helm status "${RELEASE_NAME}" \
    --namespace "${NAMESPACE}" \
    --kubeconfig "${KUBECONFIG_PATH}" \
    --kube-context "${KUBE_CONTEXT}" >/dev/null 2>&1; then
    fail "Helm release remains present after uninstall"
fi
