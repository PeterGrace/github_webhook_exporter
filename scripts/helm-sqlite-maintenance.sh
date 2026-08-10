#!/usr/bin/env bash
set -Eeuo pipefail
umask 077

readonly MAINTENANCE_IMAGE='docker.io/keinos/sqlite3:3.50.4@sha256:d9e50ca08f59d96055c514175f3f4b1fcacaca97fa93508a0334c62eb9de9382'
readonly DATABASE_FILE='github-webhook-exporter.db'

fail() {
    printf 'SQLite maintenance failed: %s\n' "$1" >&2
    exit 1
}

if [[ "$#" -ne 5 ]]; then
    printf 'usage: %s backup|restore NAMESPACE STATEFULSET PVC BACKUP_BASENAME\n' \
        "${0##*/}" >&2
    exit 2
fi

readonly OPERATION="$1"
readonly NAMESPACE="$2"
readonly STATEFULSET="$3"
readonly PVC="$4"
readonly BACKUP_BASENAME="$5"
readonly MAINTENANCE_POD="${STATEFULSET}-sqlite-maintenance"

if [[ "${OPERATION}" != backup && "${OPERATION}" != restore ]]; then
    fail 'operation must be backup or restore'
fi
for resource_name in "${NAMESPACE}" "${STATEFULSET}" "${PVC}"; do
    if (( ${#resource_name} > 63 )) || \
        [[ ! "${resource_name}" =~ ^[a-z0-9]([-a-z0-9]*[a-z0-9])?$ ]]; then
        fail 'namespace, StatefulSet, and PVC must be valid DNS labels'
    fi
done
if (( ${#MAINTENANCE_POD} > 63 )); then
    fail 'StatefulSet name is too long for the maintenance Pod name'
fi
if (( ${#BACKUP_BASENAME} > 128 )) || \
    [[ ! "${BACKUP_BASENAME}" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*\.db$ ]] || \
    [[ "${BACKUP_BASENAME}" == "${DATABASE_FILE}" ]]; then
    fail 'backup basename must be a safe, distinct .db filename'
fi
if ! command -v kubectl >/dev/null 2>&1; then
    fail 'required command not found: kubectl'
fi

KUBECTL_GLOBAL_ARGUMENTS=()
if [[ -n "${KUBECTL_CONTEXT:-}" ]]; then
    KUBECTL_GLOBAL_ARGUMENTS+=(--context "${KUBECTL_CONTEXT}")
fi
readonly -a KUBECTL_GLOBAL_ARGUMENTS

kube() {
    kubectl "${KUBECTL_GLOBAL_ARGUMENTS[@]}" "$@"
}

MAINTENANCE_NODE_NAME=''
if [[ "${OPERATION}" == backup ]]; then
    MAINTENANCE_NODE_NAME="$(kube --namespace "${NAMESPACE}" get pod "${STATEFULSET}-0" \
        --output=jsonpath='{.spec.nodeName}')" || fail 'could not inspect the exporter pod node'
    if (( ${#MAINTENANCE_NODE_NAME} > 253 )) || \
        [[ ! "${MAINTENANCE_NODE_NAME}" =~ ^[a-z0-9]([-a-z0-9.]*[a-z0-9])?$ ]]; then
        fail 'online backup requires a scheduled exporter pod'
    fi
fi
readonly MAINTENANCE_NODE_NAME

if [[ "${OPERATION}" == restore ]]; then
    # These checks are point-in-time. Keep Helm maintenance mode enabled until restore completes.
    replicas="$(kube --namespace "${NAMESPACE}" get statefulset "${STATEFULSET}" \
        --output=jsonpath='{.spec.replicas}')" || fail 'could not inspect StatefulSet replicas'
    if [[ "${replicas}" != 0 ]]; then
        fail 'restore requires the StatefulSet desired replica count to equal zero'
    fi
    if kube --namespace "${NAMESPACE}" get pod "${STATEFULSET}-0" \
        >/dev/null 2>&1; then
        fail 'restore requires the exporter pod to be fully terminated'
    fi
fi

kube --namespace "${NAMESPACE}" delete pod "${MAINTENANCE_POD}" \
    --ignore-not-found --wait=true >/dev/null

if [[ "${OPERATION}" == backup ]]; then
    read -r -d '' MAINTENANCE_COMMAND <<EOF || true
if ! sqlite3 -batch -bail /data/${DATABASE_FILE} ".backup /data/${BACKUP_BASENAME}" \
    >/dev/null 2>&1; then
    printf '%s\\n' 'maintenance_failed' >&2
    exit 1
fi
integrity="\$(sqlite3 -batch -bail /data/${BACKUP_BASENAME} \
    'PRAGMA integrity_check;' 2>/dev/null)" || {
    printf '%s\\n' 'maintenance_failed' >&2
    exit 1
}
if [ "\${integrity}" != ok ]; then
    printf '%s\\n' 'maintenance_failed' >&2
    exit 1
fi
chmod 0600 /data/${BACKUP_BASENAME}
printf '%s\\n' 'maintenance_complete'
EOF
else
    read -r -d '' MAINTENANCE_COMMAND <<EOF || true
if [ ! -f /data/${BACKUP_BASENAME} ] || [ ! -f /data/${DATABASE_FILE} ]; then
    printf '%s\\n' 'maintenance_precondition_failed' >&2
    exit 1
fi
backup_integrity="\$(sqlite3 -batch -bail /data/${BACKUP_BASENAME} \
    'PRAGMA integrity_check;' 2>/dev/null)" || {
    printf '%s\\n' 'maintenance_failed' >&2
    exit 1
}
if [ "\${backup_integrity}" != ok ]; then
    printf '%s\\n' 'maintenance_failed' >&2
    exit 1
fi
rm -f /data/${DATABASE_FILE}.restore
if ! sqlite3 -batch -bail /data/${DATABASE_FILE}.restore \
    ".restore /data/${BACKUP_BASENAME}" >/dev/null 2>&1; then
    printf '%s\\n' 'maintenance_failed' >&2
    exit 1
fi
restore_integrity="\$(sqlite3 -batch -bail /data/${DATABASE_FILE}.restore \
    'PRAGMA integrity_check;' 2>/dev/null)" || {
    printf '%s\\n' 'maintenance_failed' >&2
    exit 1
}
if [ "\${restore_integrity}" != ok ]; then
    printf '%s\\n' 'maintenance_failed' >&2
    exit 1
fi
chmod 0600 /data/${DATABASE_FILE}.restore
rm -f /data/${DATABASE_FILE}.pre-restore
mv /data/${DATABASE_FILE} /data/${DATABASE_FILE}.pre-restore
restore_original() {
    status=\$?
    if [ "\${status}" -ne 0 ] && [ -f /data/${DATABASE_FILE}.pre-restore ]; then
        rm -f /data/${DATABASE_FILE}
        mv /data/${DATABASE_FILE}.pre-restore /data/${DATABASE_FILE} || true
    fi
    exit "\${status}"
}
trap restore_original EXIT
rm -f /data/${DATABASE_FILE}-wal /data/${DATABASE_FILE}-shm
mv /data/${DATABASE_FILE}.restore /data/${DATABASE_FILE}
metadata="\$(stat -c '%u:%g %a' /data/${DATABASE_FILE})" || exit 1
if [ "\${metadata}" != '65532:65532 600' ]; then
    printf '%s\\n' 'maintenance_metadata_failed' >&2
    exit 1
fi
trap - EXIT
printf '%s\\n' 'maintenance_complete'
EOF
fi
readonly MAINTENANCE_COMMAND

kube --namespace "${NAMESPACE}" apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: ${MAINTENANCE_POD}
  labels:
    app.kubernetes.io/name: github-webhook-exporter-sqlite-maintenance
spec:
  restartPolicy: Never
  automountServiceAccountToken: false
$(if [[ -n "${MAINTENANCE_NODE_NAME}" ]]; then
    printf '  nodeName: "%s"\n' "${MAINTENANCE_NODE_NAME}"
fi)
  securityContext:
    runAsNonRoot: true
    runAsUser: 65532
    runAsGroup: 65532
    fsGroup: 65532
    fsGroupChangePolicy: OnRootMismatch
    seccompProfile:
      type: RuntimeDefault
  containers:
    - name: sqlite-maintenance
      image: ${MAINTENANCE_IMAGE}
      imagePullPolicy: IfNotPresent
      command: ["/bin/sh", "-ceu", "--"]
      args:
        - |
$(printf '%s\n' "${MAINTENANCE_COMMAND}" | sed 's/^/          /')
      workingDir: /data
      # Keep SQLite temporary files on the PVC because the root filesystem is read-only.
      env:
        - name: SQLITE_TMPDIR
          value: /data
      securityContext:
        allowPrivilegeEscalation: false
        readOnlyRootFilesystem: true
        capabilities:
          drop: ["ALL"]
      resources:
        requests:
          cpu: 10m
          memory: 16Mi
        limits:
          cpu: 250m
          memory: 128Mi
      volumeMounts:
        - name: data
          mountPath: /data
  volumes:
    - name: data
      persistentVolumeClaim:
        claimName: ${PVC}
EOF

maintenance_status=0
if ! kube --namespace "${NAMESPACE}" wait \
    --for=jsonpath='{.status.phase}'=Succeeded "pod/${MAINTENANCE_POD}" \
    --timeout=120s >/dev/null; then
    maintenance_status=1
fi
maintenance_log="$(kube --namespace "${NAMESPACE}" logs "${MAINTENANCE_POD}" \
    2>/dev/null || true)"
kube --namespace "${NAMESPACE}" delete pod "${MAINTENANCE_POD}" \
    --wait=true >/dev/null 2>&1 || true
if [[ "${maintenance_status}" -ne 0 || "${maintenance_log}" != maintenance_complete ]]; then
    fail "${OPERATION} Pod did not complete safely"
fi
printf 'SQLite %s completed for backup %s\n' "${OPERATION}" "${BACKUP_BASENAME}"
