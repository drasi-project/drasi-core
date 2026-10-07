#!/usr/bin/env bash

set -euo pipefail

org="${PACKAGE_VISIBILITY_ORG:-drasi-project}"

fail() {
    echo "::error::$*" >&2
    exit 1
}

require_token() {
    if [[ -z "${GH_TOKEN:-}" ]]; then
        fail "PACKAGES_ADMIN_TOKEN is not configured. Add a PAT classic with read:packages and access to the organization's packages; authorize organization SSO if required."
    fi
}

validate_package() {
    local package="$1"

    if ! [[ "$package" =~ ^[a-z0-9][a-z0-9._-]*(/[a-z0-9][a-z0-9._-]*)*$ ]]; then
        fail "Invalid package name: $package"
    fi
}

package_endpoint() {
    local package="$1"

    package="${package//\//%2F}"
    printf '/orgs/%s/packages/container/%s\n' "$org" "$package"
}

verify_public() {
    local package="$1"
    local endpoint
    local visibility

    endpoint="$(package_endpoint "$package")"
    if ! visibility="$(gh api --method GET "$endpoint" --jq .visibility)"; then
        fail "Unable to read package ${org}/${package}. Verify that the package exists and PACKAGES_ADMIN_TOKEN is a PAT classic with read:packages and access to the organization's packages; authorize organization SSO if required."
    fi
    case "$visibility" in
        public)
            echo "Verified ${org}/${package} is public (read-only check)."
            ;;
        private | internal)
            fail "Package ${org}/${package} is '$visibility', expected 'public'. A package administrator must open the package's GitHub Package settings and use Change visibility to select Public, then rerun verification. This script does not change visibility."
            ;;
        *)
            fail "Unexpected visibility response '$visibility' for ${org}/${package}, expected 'public'."
            ;;
    esac
}

case "${1:-}" in
    verify-public)
        [[ "$#" -ge 2 ]] || fail "Usage: $0 verify-public <package>..."
        ;;
    *)
        fail "Usage: $0 verify-public <package>..."
        ;;
esac
shift

if ! [[ "$org" =~ ^[A-Za-z0-9][A-Za-z0-9-]*$ ]]; then
    fail "Invalid organization name: $org"
fi
for package in "$@"; do
    validate_package "$package"
done

require_token
for package in "$@"; do
    verify_public "$package"
done
