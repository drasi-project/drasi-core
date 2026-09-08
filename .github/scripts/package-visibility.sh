#!/usr/bin/env bash

set -euo pipefail

org="${PACKAGE_VISIBILITY_ORG:-drasi-project}"

fail() {
    echo "::error::$*" >&2
    exit 1
}

require_token() {
    if [[ -z "${GH_TOKEN:-}" ]]; then
        fail "PACKAGES_ADMIN_TOKEN is not configured. Add a PAT classic with package administration access."
    fi
}

package_endpoint() {
    local package="$1"

    if ! [[ "$package" =~ ^[a-z0-9][a-z0-9._-]*(/[a-z0-9][a-z0-9._-]*)*$ ]]; then
        fail "Invalid package name: $package"
    fi

    package="${package//\//%2F}"
    printf '/orgs/%s/packages/container/%s\n' "$org" "$package"
}

check_access() {
    local package="$1"
    local endpoint
    local visibility

    endpoint="$(package_endpoint "$package")"
    if ! visibility="$(gh api --method PATCH "$endpoint" -f visibility=public --jq .visibility)"; then
        fail "PACKAGES_ADMIN_TOKEN cannot administer ${org}/${package}. Verify that it is an authorized PAT classic with package administration access."
    fi
    if [[ "$visibility" != "public" ]]; then
        fail "Package visibility preflight returned '$visibility' for ${org}/${package}, expected 'public'."
    fi

    echo "Package visibility access validated for ${org}/${package}."
}

set_public() {
    local package="$1"
    local endpoint
    local visibility

    endpoint="$(package_endpoint "$package")"
    if ! visibility="$(gh api "$endpoint" --jq .visibility)"; then
        fail "Unable to read package ${org}/${package}."
    fi
    if [[ "$visibility" == "public" ]]; then
        echo "${org}/${package} is already public."
        return
    fi

    echo "Setting ${org}/${package} to public..."
    if ! visibility="$(gh api --method PATCH "$endpoint" -f visibility=public --jq .visibility)"; then
        fail "Failed to set ${org}/${package} to public."
    fi
    if [[ "$visibility" != "public" ]]; then
        fail "Package ${org}/${package} remained '$visibility' after the visibility update."
    fi
}

require_token

case "${1:-}" in
    check)
        [[ "$#" -eq 2 ]] || fail "Usage: $0 check <package>"
        check_access "$2"
        ;;
    set-public)
        [[ "$#" -ge 2 ]] || fail "Usage: $0 set-public <package>..."
        shift
        for package in "$@"; do
            set_public "$package"
        done
        ;;
    *)
        fail "Usage: $0 {check <package>|set-public <package>...}"
        ;;
esac
