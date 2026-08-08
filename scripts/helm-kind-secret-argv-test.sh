#!/usr/bin/env bash
set -Eeuo pipefail

if [[ "$#" -ne 2 || -z "$1" || -z "$2" ]]; then
    printf 'usage: %s ACCEPTANCE_SCRIPT CHART_DIRECTORY\n' "${0##*/}" >&2
    exit 2
fi
readonly ACCEPTANCE_SCRIPT="$1"
readonly CHART_DIRECTORY="$2"

if [[ ! -x "${ACCEPTANCE_SCRIPT}" ]]; then
    printf 'acceptance script is not executable: %s\n' "${ACCEPTANCE_SCRIPT}" >&2
    exit 2
fi
if [[ ! -d "${CHART_DIRECTORY}" || ! -f "${CHART_DIRECTORY}/Chart.yaml" ]]; then
    printf 'invalid Helm chart directory: %s\n' "${CHART_DIRECTORY}" >&2
    exit 2
fi

for command_name in cat chmod mktemp rm stat; do
    if ! command -v "${command_name}" >/dev/null 2>&1; then
        printf 'required command not found: %s\n' "${command_name}" >&2
        exit 2
    fi
done

TEMPORARY_DIRECTORY="$(mktemp -d)"
readonly TEMPORARY_DIRECTORY
readonly FAKE_BIN_DIRECTORY="${TEMPORARY_DIRECTORY}/bin"
readonly SECRET_CREATION_MARKER="${TEMPORARY_DIRECTORY}/secret-created"
mkdir "${FAKE_BIN_DIRECTORY}"

cleanup() {
    rm -rf "${TEMPORARY_DIRECTORY}"
}
trap cleanup EXIT

cat >"${FAKE_BIN_DIRECTORY}/kind" <<'EOF'
#!/usr/bin/env bash
set -Eeuo pipefail

case "${1:-}" in
    get | create | delete)
        exit 0
        ;;
    *)
        printf 'unexpected fake kind invocation: %s\n' "$*" >&2
        exit 64
        ;;
esac
EOF

cat >"${FAKE_BIN_DIRECTORY}/helm" <<'EOF'
#!/usr/bin/env bash
set -Eeuo pipefail

for argument in "$@"; do
    if [[ "${argument}" == '--wait=false' ]]; then
        printf 'deprecated Helm boolean wait argument is not permitted\n' >&2
        exit 69
    fi
done

case "${1:-}" in
    install | uninstall | list)
        exit 0
        ;;
    *)
        printf 'unexpected fake helm invocation: %s\n' "$*" >&2
        exit 64
        ;;
esac
EOF

cat >"${FAKE_BIN_DIRECTORY}/kubectl" <<'EOF'
#!/usr/bin/env bash
set -Eeuo pipefail

master_key_file=''
admin_token_file=''
is_secret_creation=false
jsonpath=''

for argument in "$@"; do
    case "${argument}" in
        --from-literal=master-key=* | --from-literal=admin-token=*)
            printf 'Secret values must not be passed through kubectl arguments\n' >&2
            exit 65
            ;;
        --from-file=master-key=*)
            master_key_file="${argument#--from-file=master-key=}"
            ;;
        --from-file=admin-token=*)
            admin_token_file="${argument#--from-file=admin-token=}"
            ;;
        --output=jsonpath=*)
            jsonpath="${argument#--output=jsonpath=}"
            ;;
        secret)
            is_secret_creation=true
            ;;
    esac
done

if [[ "${is_secret_creation}" == true ]]; then
    for secret_file in "${master_key_file}" "${admin_token_file}"; do
        if [[ -z "${secret_file}" || ! -s "${secret_file}" ]]; then
            printf 'Secret creation requires non-empty --from-file inputs\n' >&2
            exit 66
        fi
        if [[ "$(stat -c '%a' "${secret_file}")" != 600 ]]; then
            printf 'Secret input file must have mode 600: %s\n' "${secret_file}" >&2
            exit 67
        fi
    done
    : >"${SECRET_CREATION_MARKER}"
fi

case "${jsonpath}" in
    "{.spec.replicas}")
        printf '1'
        ;;
    "{.spec.template.spec.securityContext.runAsUser}")
        printf '65532'
        ;;
    '')
        ;;
    *)
        printf 'unexpected fake kubectl jsonpath: %s\n' "${jsonpath}" >&2
        exit 68
        ;;
esac
EOF

chmod 700 \
    "${FAKE_BIN_DIRECTORY}/kind" \
    "${FAKE_BIN_DIRECTORY}/helm" \
    "${FAKE_BIN_DIRECTORY}/kubectl"

export PATH="${FAKE_BIN_DIRECTORY}:${PATH}"
export SECRET_CREATION_MARKER
"${ACCEPTANCE_SCRIPT}" "${CHART_DIRECTORY}"

if [[ ! -f "${SECRET_CREATION_MARKER}" ]]; then
    printf 'acceptance script did not exercise Secret creation\n' >&2
    exit 1
fi
