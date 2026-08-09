#!/usr/bin/env bash
set -Eeuo pipefail

readonly INSTALL_DIRECTORY="${1:-}"
SCRIPT_DIRECTORY="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIRECTORY
readonly TOOL_VERSIONS_FILE="${SCRIPT_DIRECTORY}/../ci/tool-versions.env"
readonly EXPECTED_KEYS=(
    HELM_VERSION
    HELM_SHA256
    KUBECONFORM_VERSION
    KUBECONFORM_SHA256
    CONFTEST_VERSION
    CONFTEST_SHA256
    YQ_VERSION
    YQ_SHA256
    SHELLCHECK_VERSION
    SHELLCHECK_SHA256
    JUST_VERSION
    JUST_SHA256
    RUST_VERSION
)
readonly SUPPORTED_ARCHITECTURE="amd64"

TEMPORARY_DIRECTORY=""

fail() {
    printf 'CI tool installer failed: %s\n' "$1" >&2
    exit 1
}

usage() {
    printf 'usage: %s INSTALL_DIRECTORY\n' "${0##*/}" >&2
    exit 2
}

require_command() {
    local command_name="$1"
    if ! command -v "${command_name}" >/dev/null 2>&1; then
        fail "required command not found: ${command_name}"
    fi
}

cleanup() {
    if [[ -n "${TEMPORARY_DIRECTORY}" ]]; then
        rm -rf -- "${TEMPORARY_DIRECTORY}"
    fi
}

trap cleanup EXIT

parse_tool_versions() {
    declare -gA TOOL_VERSIONS=()
    declare -gA TOOL_HASHES=()

    local line key value
    while IFS= read -r line || [[ -n "${line}" ]]; do
        case "${line}" in
            ""|\#*)
                continue
                ;;
        esac

        if [[ ! "${line}" =~ ^([A-Z0-9_]+)=([A-Za-z0-9.]+)$ ]]; then
            fail "invalid tool version entry: ${line}"
        fi

        key="${BASH_REMATCH[1]}"
        value="${BASH_REMATCH[2]}"

        case " ${EXPECTED_KEYS[*]} " in
            *" ${key} "*)
                ;;
            *)
                fail "unexpected tool version key: ${key}"
                ;;
        esac

        if [[ -n "${TOOL_VERSIONS[${key}]+x}" ]]; then
            fail "duplicate tool version key: ${key}"
        fi

        if [[ "${key}" == *_SHA256 ]]; then
            if [[ ! "${value}" =~ ^[0-9a-f]{64}$ ]]; then
                fail "invalid checksum for ${key}"
            fi
            TOOL_HASHES[${key%_SHA256}]="${value}"
            continue
        fi

        if [[ "${key}" == RUST_VERSION ]]; then
            if [[ ! "${value}" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
                fail "invalid Rust version: ${value}"
            fi
        else
            if [[ ! "${value}" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
                fail "invalid tool version: ${key}=${value}"
            fi
        fi

        TOOL_VERSIONS[${key}]="${value}"
    done < "${TOOL_VERSIONS_FILE}"

    local expected_key
    for expected_key in "${EXPECTED_KEYS[@]}"; do
        if [[ -z "${TOOL_VERSIONS[${expected_key}]+x}" && ${expected_key} != *_SHA256 ]]; then
            fail "missing required tool version key: ${expected_key}"
        fi
    done

    for expected_key in HELM KUBECONFORM CONFTEST YQ SHELLCHECK JUST; do
        if [[ -z "${TOOL_HASHES[${expected_key}]+x}" ]]; then
            fail "missing required checksum key: ${expected_key}_SHA256"
        fi
    done
}

resolve_architecture() {
    case "$(uname -m)" in
        x86_64|amd64)
            printf '%s\n' "${SUPPORTED_ARCHITECTURE}"
            ;;
        *)
            fail "unsupported architecture: $(uname -m)"
            ;;
    esac
}

download_release_asset() {
    local url="$1"
    local checksum="$2"
    local output_path="$3"

    local archive_name
    archive_name="${output_path##*/}"

    curl -fsSL --retry 3 --retry-delay 1 --output "${output_path}" "${url}"

    local checksum_file="${TEMPORARY_DIRECTORY}/${archive_name}.sha256"
    printf '%s  %s\n' "${checksum}" "${archive_name}" > "${checksum_file}"
    (cd "$(dirname "${output_path}")" && sha256sum --check --status "${checksum_file}")
}

extract_expected_member() {
    local archive_path="$1"
    local member_path="$2"
    local output_path="$3"

    local listed_member
    listed_member="$(tar -tf "${archive_path}" | awk -v member="${member_path}" '
        $0 == member { found = $0 }
        END {
            if (found != "") {
                print found
            }
        }
    ')"

    if [[ -z "${listed_member}" ]]; then
        fail "missing expected archive member: ${member_path}"
    fi

    tar -xOf "${archive_path}" "${listed_member}" > "${output_path}"
    chmod 0755 "${output_path}"
}

install_helm() {
    local version="$1"
    local checksum="$2"
    local architecture="$3"
    local archive_path="${TEMPORARY_DIRECTORY}/helm.tar.gz"
    local url="https://get.helm.sh/helm-v${version}-linux-${architecture}.tar.gz"

    download_release_asset "${url}" "${checksum}" "${archive_path}"
    extract_expected_member "${archive_path}" "linux-${architecture}/helm" "${INSTALL_DIRECTORY}/helm"
}

install_kubeconform() {
    local version="$1"
    local checksum="$2"
    local architecture="$3"
    local archive_path="${TEMPORARY_DIRECTORY}/kubeconform.tar.gz"
    local url="https://github.com/yannh/kubeconform/releases/download/v${version}/kubeconform-linux-${architecture}.tar.gz"

    download_release_asset "${url}" "${checksum}" "${archive_path}"
    extract_expected_member "${archive_path}" "kubeconform" "${INSTALL_DIRECTORY}/kubeconform"
}

install_conftest() {
    local version="$1"
    local checksum="$2"
    local archive_path="${TEMPORARY_DIRECTORY}/conftest.tar.gz"
    local url="https://github.com/open-policy-agent/conftest/releases/download/v${version}/conftest_${version}_Linux_x86_64.tar.gz"

    download_release_asset "${url}" "${checksum}" "${archive_path}"
    extract_expected_member "${archive_path}" "conftest" "${INSTALL_DIRECTORY}/conftest"
}

install_yq() {
    local version="$1"
    local checksum="$2"
    local binary_path="${TEMPORARY_DIRECTORY}/yq"
    local url="https://github.com/mikefarah/yq/releases/download/v${version}/yq_linux_amd64"

    download_release_asset "${url}" "${checksum}" "${binary_path}"
    chmod 0755 "${binary_path}"
    mv -- "${binary_path}" "${INSTALL_DIRECTORY}/yq"
}

install_shellcheck() {
    local version="$1"
    local checksum="$2"
    local archive_path="${TEMPORARY_DIRECTORY}/shellcheck.tar.xz"
    local url="https://github.com/koalaman/shellcheck/releases/download/v${version}/shellcheck-v${version}.linux.x86_64.tar.xz"

    download_release_asset "${url}" "${checksum}" "${archive_path}"
    extract_expected_member "${archive_path}" "shellcheck-v${version}/shellcheck" "${INSTALL_DIRECTORY}/shellcheck"
}

install_just() {
    local version="$1"
    local checksum="$2"
    local archive_path="${TEMPORARY_DIRECTORY}/just.tar.gz"
    local url="https://github.com/casey/just/releases/download/${version}/just-${version}-x86_64-unknown-linux-musl.tar.gz"

    download_release_asset "${url}" "${checksum}" "${archive_path}"
    extract_expected_member "${archive_path}" "just" "${INSTALL_DIRECTORY}/just"
}

install_rust_toolchain() {
    local version="$1"

    rustup -q toolchain install "${version}" --profile minimal --component rustfmt --component clippy --no-self-update >/dev/null 2>&1
}

print_version_output() {
    local version="$1"
    local command_path="$2"
    shift 2

    rustup run "${version}" "${command_path}" "$@"
}

main() {
    if [[ -z "${INSTALL_DIRECTORY}" ]]; then
        usage
    fi

    require_command awk
    require_command chmod
    require_command curl
    require_command mktemp
    require_command rm
    require_command rustup
    require_command sha256sum
    require_command tar
    require_command uname

    if [[ ! -f "${TOOL_VERSIONS_FILE}" ]]; then
        fail "missing tool version file: ${TOOL_VERSIONS_FILE}"
    fi

    mkdir -p -- "${INSTALL_DIRECTORY}"
    local write_probe
    write_probe="$(mktemp "${INSTALL_DIRECTORY}/.write-probe.XXXXXX")"
    rm -f -- "${write_probe}"

    TEMPORARY_DIRECTORY="$(mktemp -d)"

    parse_tool_versions

    local architecture
    architecture="$(resolve_architecture)"

    install_helm "${TOOL_VERSIONS[HELM_VERSION]}" "${TOOL_HASHES[HELM]}" "${architecture}"
    install_kubeconform "${TOOL_VERSIONS[KUBECONFORM_VERSION]}" "${TOOL_HASHES[KUBECONFORM]}" "${architecture}"
    install_conftest "${TOOL_VERSIONS[CONFTEST_VERSION]}" "${TOOL_HASHES[CONFTEST]}" "${architecture}"
    install_yq "${TOOL_VERSIONS[YQ_VERSION]}" "${TOOL_HASHES[YQ]}" "${architecture}"
    install_shellcheck "${TOOL_VERSIONS[SHELLCHECK_VERSION]}" "${TOOL_HASHES[SHELLCHECK]}" "${architecture}"
    install_just "${TOOL_VERSIONS[JUST_VERSION]}" "${TOOL_HASHES[JUST]}" "${architecture}"
    install_rust_toolchain "${TOOL_VERSIONS[RUST_VERSION]}"

    print_version_output "${TOOL_VERSIONS[RUST_VERSION]}" rustc --version
    print_version_output "${TOOL_VERSIONS[RUST_VERSION]}" cargo --version
    print_version_output "${TOOL_VERSIONS[RUST_VERSION]}" rustfmt --version
    print_version_output "${TOOL_VERSIONS[RUST_VERSION]}" clippy-driver --version
    "${INSTALL_DIRECTORY}/helm" version --short
    "${INSTALL_DIRECTORY}/kubeconform" -v
    "${INSTALL_DIRECTORY}/conftest" --version
    "${INSTALL_DIRECTORY}/yq" --version
    "${INSTALL_DIRECTORY}/shellcheck" --version
    "${INSTALL_DIRECTORY}/just" --version
}

main "$@"
