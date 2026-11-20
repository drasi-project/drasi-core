#!/usr/bin/env bash

set -euo pipefail

org="${PACKAGE_VISIBILITY_ORG:-drasi-project}"

fail() {
    echo "::error::$*" >&2
    exit 1
}

validate_package() {
    local package="$1"

    if ! [[ "$package" =~ ^[a-z0-9][a-z0-9._-]*(/[a-z0-9][a-z0-9._-]*)*$ ]]; then
        fail "Invalid package name: $package"
    fi
}

http_get() {
    local url="$1"
    local accept="$2"
    local token="${3:-}"
    local result

    if ! result="$(
        {
            printf 'header = "Accept: %s"\n' "$accept"
            if [[ -n "$token" ]]; then
                printf 'header = "Authorization: Bearer %s"\n' "$token"
            fi
        } | curl --disable --silent --show-error --request GET --location \
            --proto '=https' --proto-redir '=https' --max-redirs 3 \
            --connect-timeout 10 --max-time 30 --max-filesize 1048576 \
            --output "$response_file" --write-out '%{http_code}\n%{content_type}' \
            --config - "$url"
    )"; then
        return 1
    fi

    if [[ "$result" != *$'\n'* ]]; then
        return 1
    fi
    http_status="${result%%$'\n'*}"
    http_content_type="${result#*$'\n'}"
    http_status="${http_status//$'\r'/}"
    http_content_type="${http_content_type//$'\r'/}"
    http_content_type="${http_content_type%%;*}"
}

extract_pull_token() {
    python3 - "$1" <<'PY'
import json
import re
import sys

value = json.load(open(sys.argv[1], encoding="utf-8")).get("token")
if not isinstance(value, str) or not re.fullmatch(r"[A-Za-z0-9._~+/=-]{1,16384}", value):
    raise ValueError("invalid anonymous pull token")
print(value)
PY
}

select_tag() {
    python3 - "$1" <<'PY'
import json
import re
import sys

value = json.load(open(sys.argv[1], encoding="utf-8"))
tags = value.get("tags") if isinstance(value, dict) else None
if not isinstance(tags, list):
    raise ValueError("invalid GHCR tag list")
tags = sorted(tag for tag in tags if isinstance(tag, str) and
              re.fullmatch(r"[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}", tag))
if not tags:
    raise ValueError("no valid GHCR tags")
print(tags[0])
PY
}

validate_manifest() {
    python3 - "$1" <<'PY'
import json
import sys

value = json.load(open(sys.argv[1], encoding="utf-8"))
if not isinstance(value, dict) or value.get("schemaVersion") != 2:
    raise ValueError("invalid OCI manifest")
PY
}

verify_public() {
    local package="$1"
    local repository="${org}/${package}"
    local token_url="https://ghcr.io/token?service=ghcr.io&scope=repository:${repository}:pull"
    local token
    local tags_url="https://ghcr.io/v2/${repository}/tags/list"
    local tag
    local manifest_url

    if ! http_get "$token_url" application/json; then
        fail "Unable to request an anonymous GHCR pull token for ${repository}."
    fi
    if [[ "$http_status" != "200" || "$http_content_type" != "application/json" ]]; then
        fail "Unable to request an anonymous GHCR pull token for ${repository} (HTTP ${http_status})."
    fi
    if ! token="$(extract_pull_token "$response_file" 2>/dev/null)"; then
        fail "GHCR returned an invalid anonymous pull token for ${repository}."
    fi

    if ! http_get "$tags_url" application/json "$token"; then
        fail "Unable to list tags for ${repository} through anonymous GHCR access."
    fi
    if [[ "$http_status" != "200" || "$http_content_type" != "application/json" ]]; then
        fail "Package ${repository} is missing or not anonymously pullable (tag listing returned HTTP ${http_status})."
    fi
    if ! tag="$(select_tag "$response_file" 2>/dev/null)"; then
        fail "Package ${repository} has no valid anonymously accessible tags."
    fi

    manifest_url="https://ghcr.io/v2/${repository}/manifests/${tag}"
    if ! http_get "$manifest_url" \
        "application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json" \
        "$token"; then
        fail "Unable to pull a manifest for ${repository} through anonymous GHCR access."
    fi
    if [[ "$http_status" != "200" ]]; then
        fail "Package ${repository} is missing or not anonymously pullable (manifest returned HTTP ${http_status})."
    fi
    case "$http_content_type" in
        application/vnd.oci.image.index.v1+json | \
        application/vnd.oci.image.manifest.v1+json | \
        application/vnd.docker.distribution.manifest.list.v2+json | \
        application/vnd.docker.distribution.manifest.v2+json)
            ;;
        *)
            fail "Package ${repository} returned an unexpected manifest content type."
            ;;
    esac
    if ! validate_manifest "$response_file" >/dev/null 2>&1; then
        fail "Package ${repository} returned an invalid OCI manifest."
    fi

    echo "Verified ${repository} is anonymously pullable (read-only check)."
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

response_dir="$(mktemp -d)"
trap 'rm -rf "$response_dir"' EXIT
response_file="$response_dir/response"
for package in "$@"; do
    verify_public "$package"
done
