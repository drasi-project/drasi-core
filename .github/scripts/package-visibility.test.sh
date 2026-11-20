#!/usr/bin/env bash

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
visibility_script="$script_dir/package-visibility.sh"
test_dir="$(mktemp -d)"
trap 'rm -rf "$test_dir"' EXIT

mock_bin="$test_dir/bin"
mkdir -p "$mock_bin"

cat >"$mock_bin/curl" <<'EOF'
#!/usr/bin/env bash

set -euo pipefail

output_file=""
url=""
method=""
while [[ "$#" -gt 0 ]]; do
    case "$1" in
        --output | --write-out | --config | --request | --proto | --proto-redir | --max-redirs | \
        --connect-timeout | --max-time | --max-filesize)
        if [[ "$1" == "--request" && "$2" != "GET" ]]; then
            echo "Only GET requests are supported" >&2
            exit 1
        fi
        if [[ "$1" == "--request" ]]; then
            method="$2"
        fi
            if [[ "$1" == "--output" ]]; then
                output_file="$2"
            fi
            shift 2
            ;;
        *)
            if [[ "$1" == https://* ]]; then
                url="$1"
            fi
            shift
            ;;
    esac
done
if [[ "$method" != "GET" ]]; then
    echo "An explicit GET request is required" >&2
    exit 1
fi
config="$(cat)"
printf '%s\n' "$url" >>"$MOCK_CALLS"

if [[ "$url" == https://ghcr.io/token?* ]]; then
    if [[ "$config" == *"Authorization: Bearer"* ]]; then
        echo "Token request must be anonymous" >&2
        exit 1
    fi
    case "$MOCK_SCENARIO" in
        token_denied)
            body='{"error":"denied"}'
            status=403
            ;;
        invalid_token)
            body='{"token":"invalid token"}'
            status=200
            ;;
        *)
            body='{"token":"anonymous-test-token"}'
            status=200
            ;;
    esac
    content_type=application/json
elif [[ "$url" == */tags/list ]]; then
    if [[ "$config" != *"Authorization: Bearer anonymous-test-token"* ]]; then
        echo "Tag listing must use the anonymous pull token" >&2
        exit 1
    fi
    case "$MOCK_SCENARIO" in
        denied)
            body='{"errors":[{"code":"DENIED"}]}'
            status=401
            ;;
        missing)
            body='{"errors":[{"code":"NAME_UNKNOWN"}]}'
            status=404
            ;;
        empty_tags)
            body='{"name":"drasi-project/example","tags":[]}'
            status=200
            ;;
        invalid_tags)
            body='{"name":"drasi-project/example","tags":["../bad"]}'
            status=200
            ;;
        *)
            body='{"name":"drasi-project/example","tags":["v1"]}'
            status=200
            ;;
    esac
    content_type=application/json
elif [[ "$url" == */manifests/* ]]; then
    if [[ "$config" != *"Authorization: Bearer anonymous-test-token"* ]]; then
        echo "Manifest request must use the anonymous pull token" >&2
        exit 1
    fi
    case "$MOCK_SCENARIO" in
        manifest_denied)
            body='{"errors":[{"code":"DENIED"}]}'
            status=403
            content_type=application/json
            ;;
        invalid_manifest)
            body='{"schemaVersion":1}'
            status=200
            content_type=application/vnd.oci.image.manifest.v1+json
            ;;
        bad_content_type)
            body='{"schemaVersion":2}'
            status=200
            content_type=application/json
            ;;
        *)
            body='{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}'
            status=200
            content_type=application/vnd.oci.image.manifest.v1+json
            ;;
    esac
else
    echo "Unexpected request URL: $url" >&2
    exit 1
fi

printf '%s' "$body" >"$output_file"
printf '%s\n%s' "$status" "$content_type"
EOF
chmod +x "$mock_bin/curl"

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

assert_contains() {
    local text="$1"
    local expected="$2"
    [[ "$text" == *"$expected"* ]] || fail "expected output to contain: $expected"
}

expect_failure() {
    if output="$("$@" 2>&1)"; then
        fail "expected failure: $*"
    fi
}

assert_requests() {
    local calls="$1"
    shift
    local actual
    actual="$(cat "$calls")"
    local expected
    expected="$(printf '%s\n' "$@")"
    [[ "$actual" == "$expected" ]] || fail "unexpected GHCR requests: $actual"
}

run_mock() {
    local scenario="$1"
    local calls="$2"
    shift 2

    env \
        PATH="$mock_bin:$PATH" \
        GH_TOKEN="" \
        PACKAGE_VISIBILITY_ORG="${PACKAGE_VISIBILITY_ORG:-drasi-project}" \
        MOCK_SCENARIO="$scenario" \
        MOCK_CALLS="$calls" \
        "$visibility_script" "$@"
}

export PACKAGE_VISIBILITY_ORG=drasi-project
base_url="https://ghcr.io/v2/drasi-project"

calls="$test_dir/public-calls"
output="$(run_mock public "$calls" verify-public drasi-plugin-directory)"
assert_contains "$output" "Verified drasi-project/drasi-plugin-directory is anonymously pullable"
assert_requests "$calls" \
    "https://ghcr.io/token?service=ghcr.io&scope=repository:drasi-project/drasi-plugin-directory:pull" \
    "$base_url/drasi-plugin-directory/tags/list" \
    "$base_url/drasi-plugin-directory/manifests/v1"
[[ "$(wc -l <"$calls")" -eq 3 ]] || fail "a public package should use one token, tag-list and manifest GET"

for scenario in token_denied invalid_token; do
    calls="$test_dir/$scenario-calls"
    expect_failure run_mock "$scenario" "$calls" verify-public drasi-plugin-directory
    if [[ "$scenario" == token_denied ]]; then
        assert_contains "$output" "anonymous GHCR pull token"
    else
        assert_contains "$output" "invalid anonymous pull token"
    fi
    [[ "$(wc -l <"$calls")" -eq 1 ]] || fail "invalid token responses should stop before package requests"
done

for scenario in denied missing; do
    calls="$test_dir/$scenario-calls"
    expect_failure run_mock "$scenario" "$calls" verify-public drasi-plugin-directory
    assert_contains "$output" "missing or not anonymously pullable"
    assert_requests "$calls" \
        "https://ghcr.io/token?service=ghcr.io&scope=repository:drasi-project/drasi-plugin-directory:pull" \
        "$base_url/drasi-plugin-directory/tags/list"
done

for scenario in manifest_denied; do
    calls="$test_dir/$scenario-calls"
    expect_failure run_mock "$scenario" "$calls" verify-public drasi-plugin-directory
    assert_contains "$output" "missing or not anonymously pullable"
    assert_requests "$calls" \
        "https://ghcr.io/token?service=ghcr.io&scope=repository:drasi-project/drasi-plugin-directory:pull" \
        "$base_url/drasi-plugin-directory/tags/list" \
        "$base_url/drasi-plugin-directory/manifests/v1"
done

for scenario in empty_tags invalid_tags; do
    calls="$test_dir/$scenario-calls"
    expect_failure run_mock "$scenario" "$calls" verify-public drasi-plugin-directory
    assert_contains "$output" "no valid anonymously accessible tags"
    [[ "$(wc -l <"$calls")" -eq 2 ]] || fail "invalid tags should stop before manifest requests"
done

for scenario in invalid_manifest bad_content_type; do
    calls="$test_dir/$scenario-calls"
    expect_failure run_mock "$scenario" "$calls" verify-public drasi-plugin-directory
    assert_requests "$calls" \
        "https://ghcr.io/token?service=ghcr.io&scope=repository:drasi-project/drasi-plugin-directory:pull" \
        "$base_url/drasi-plugin-directory/tags/list" \
        "$base_url/drasi-plugin-directory/manifests/v1"
    if [[ "$scenario" == invalid_manifest ]]; then
        assert_contains "$output" "invalid OCI manifest"
    else
        assert_contains "$output" "unexpected manifest content type"
    fi
done

calls="$test_dir/packages-calls"
output="$(run_mock public "$calls" verify-public drasi-plugin-directory source/http index/rocksdb)"
assert_contains "$output" "Verified drasi-project/index/rocksdb is anonymously pullable"
[[ "$(wc -l <"$calls")" -eq 9 ]] || fail "each valid package should be checked with anonymous GHCR requests"
assert_requests "$calls" \
    "https://ghcr.io/token?service=ghcr.io&scope=repository:drasi-project/drasi-plugin-directory:pull" \
    "$base_url/drasi-plugin-directory/tags/list" \
    "$base_url/drasi-plugin-directory/manifests/v1" \
    "https://ghcr.io/token?service=ghcr.io&scope=repository:drasi-project/source/http:pull" \
    "$base_url/source/http/tags/list" \
    "$base_url/source/http/manifests/v1" \
    "https://ghcr.io/token?service=ghcr.io&scope=repository:drasi-project/index/rocksdb:pull" \
    "$base_url/index/rocksdb/tags/list" \
    "$base_url/index/rocksdb/manifests/v1"

calls="$test_dir/org-calls"
output="$(PACKAGE_VISIBILITY_ORG=other-org run_mock public "$calls" verify-public source/http)"
assert_contains "$output" "Verified other-org/source/http is anonymously pullable"
assert_requests "$calls" \
    "https://ghcr.io/token?service=ghcr.io&scope=repository:other-org/source/http:pull" \
    "https://ghcr.io/v2/other-org/source/http/tags/list" \
    "https://ghcr.io/v2/other-org/source/http/manifests/v1"

calls="$test_dir/invalid-calls"
for package in '' 'source/../invalid' '/source/http' 'source//http' 'source/http/' \
    'Source/http' 'source/http?visibility=public' 'source%2Fhttp' 'source/http log'; do
    expect_failure run_mock public "$calls" verify-public source/http "$package"
    assert_contains "$output" "Invalid package name"
    [[ ! -e "$calls" ]] || fail "all package names should be validated before calling GHCR"
done

calls="$test_dir/usage-calls"
for command in verify-public check set-public unknown; do
    expect_failure run_mock public "$calls" "$command"
    assert_contains "$output" "Usage:"
done
expect_failure run_mock public "$calls"
assert_contains "$output" "Usage:"
expect_failure run_mock public "$calls" set-public source/http
assert_contains "$output" "Usage:"
[[ ! -e "$calls" ]] || fail "invalid arguments should fail before calling GHCR"

calls="$test_dir/invalid-org-calls"
PACKAGE_VISIBILITY_ORG=../invalid expect_failure run_mock public "$calls" verify-public source/http
assert_contains "$output" "Invalid organization name"
[[ ! -e "$calls" ]] || fail "invalid organization should fail before calling GHCR"

echo "package-visibility tests passed"
