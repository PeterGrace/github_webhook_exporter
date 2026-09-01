#!/usr/bin/env bash
set -Eeuo pipefail

readonly VERSION="${1:-}"
# GITHUB_REPOSITORY keeps the compare link correct on a fork without plumbing an argument
# through the workflow; the constant only applies to local previews.
readonly REPOSITORY="${2:-${GITHUB_REPOSITORY:-PeterGrace/github_webhook_exporter}}"
readonly RELEASE_TAG="v${VERSION}"
readonly IMAGE_REPOSITORY="ghcr.io/petergrace/github-webhook-exporter"
readonly CHART_REFERENCE="oci://ghcr.io/petergrace/charts/github-webhook-exporter"
# Stable release tags only. Prerelease and non-canonical tags never gate a published release, so
# they must not become the comparison base either.
readonly STABLE_TAG_PATTERN='^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'
# git log records are emitted with a unit separator so a subject may contain any printable text.
readonly FIELD_SEPARATOR=$'\x1f'

PREVIOUS_TAG=""
# Bash associative array keyed by section bucket; each value accumulates rendered bullet lines.
declare -A SECTION_ENTRIES=()

fail() {
    printf 'release changelog generation failed: %s\n' "$1" >&2
    exit 1
}

usage() {
    printf 'usage: %s VERSION [REPOSITORY]\n' "${0##*/}" >&2
    exit 2
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || fail "required command not found: $1"
}

validate_inputs() {
    [[ $# -ge 1 && $# -le 2 ]] || usage
    [[ "${VERSION}" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] \
        || fail "version must be a canonical semantic version"
    [[ "${REPOSITORY}" =~ ^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$ ]] \
        || fail "repository must be in OWNER/NAME form"

    require_command git
    require_command sort

    git rev-parse -q --verify "refs/tags/${RELEASE_TAG}^{commit}" >/dev/null \
        || fail "release tag ${RELEASE_TAG} is not present in this checkout"
}

# Select the newest stable tag that the release tag actually descends from. Ancestry matters
# because an unrelated or abandoned tag would otherwise produce a nonsensical commit range.
select_previous_tag() {
    local candidate
    while read -r candidate; do
        [[ "${candidate}" =~ ${STABLE_TAG_PATTERN} ]] || continue
        [[ "${candidate}" != "${RELEASE_TAG}" ]] || continue
        if git merge-base --is-ancestor "${candidate}^{commit}" "${RELEASE_TAG}^{commit}"; then
            PREVIOUS_TAG="${candidate}"
            return 0
        fi
    done < <(git tag --list 'v*' | sort --version-sort --reverse)
}

commit_range() {
    if [[ -n "${PREVIOUS_TAG}" ]]; then
        printf '%s..%s\n' "${PREVIOUS_TAG}" "${RELEASE_TAG}"
    else
        printf '%s\n' "${RELEASE_TAG}"
    fi
}

# Split each landed commit's "type(scope)!: description" subject into a section bucket and a
# bullet. Non-conventional subjects keep their whole subject and land in the "other" bucket.
collect_entries() {
    local record abbreviated subject bucket description

    while IFS= read -r record; do
        abbreviated="${record%%"${FIELD_SEPARATOR}"*}"
        subject="${record#*"${FIELD_SEPARATOR}"}"
        # The cargo-release version bump describes the release itself, not a change within it.
        [[ "${subject}" != "chore: Release "* ]] || continue

        if [[ "${subject}" =~ ^([a-z]+)(\(([^\)]*)\))?(!)?:[[:space:]]+(.+)$ ]]; then
            bucket="${BASH_REMATCH[1]}"
            description="${BASH_REMATCH[5]}"
            if [[ -n "${BASH_REMATCH[3]}" ]]; then
                description="(${BASH_REMATCH[3]}) ${description}"
            fi
            if [[ -n "${BASH_REMATCH[4]}" ]]; then
                bucket="breaking"
            fi
        else
            bucket="other"
            description="${subject}"
        fi

        SECTION_ENTRIES["${bucket}"]+="- ${description} (${abbreviated})"$'\n'
    done < <(git log --no-merges --format="%h${FIELD_SEPARATOR}%s" "$(commit_range)")
}

render_section() {
    local bucket="$1"
    local heading="$2"

    [[ -n "${SECTION_ENTRIES["${bucket}"]:-}" ]] || return 0
    printf '## %s\n\n%s\n' "${heading}" "${SECTION_ENTRIES["${bucket}"]}"
}

render_notes() {
    printf '## Install\n\n'
    printf '```bash\n'
    printf 'docker pull %s:%s\n' "${IMAGE_REPOSITORY}" "${VERSION}"
    printf 'helm install github-webhook-exporter %s --version %s\n' \
        "${CHART_REFERENCE}" "${VERSION}"
    printf '```\n\n'
    printf 'Published version tags are immutable. The attached chart archive is a convenience copy\n'
    printf 'of the published OCI chart.\n\n'

    render_section breaking 'Breaking changes'
    render_section feat 'Features'
    render_section fix 'Fixes'
    render_section perf 'Performance'
    render_section refactor 'Refactoring'
    render_section docs 'Documentation'
    render_section test 'Testing'
    render_section build 'Build'
    render_section ci 'Continuous integration'
    render_section chore 'Chores'
    render_section style 'Style'
    render_section revert 'Reverts'
    render_section other 'Other changes'

    if [[ -n "${PREVIOUS_TAG}" ]]; then
        printf '**Full changelog**: https://github.com/%s/compare/%s...%s\n' \
            "${REPOSITORY}" "${PREVIOUS_TAG}" "${RELEASE_TAG}"
    else
        printf '**Full changelog**: https://github.com/%s/commits/%s\n' \
            "${REPOSITORY}" "${RELEASE_TAG}"
    fi
}

main() {
    validate_inputs "$@"
    select_previous_tag
    collect_entries

    if (( ${#SECTION_ENTRIES[@]} == 0 )); then
        fail "no landed commits found for ${RELEASE_TAG}"
    fi

    render_notes
}

main "$@"
