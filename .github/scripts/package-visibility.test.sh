#!/usr/bin/env bash

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
visibility_script="$script_dir/package-visibility.sh"
test_dir="$(mktemp -d)"
trap 'rm -rf "$test_dir"' EXIT

mock_bin="$test_dir/bin"
mkdir -p "$mock_bin"

cat >"$mock_bin/gh" <<'EOF'
#!/usr/bin/env bash

set -euo pipefail

printf '%s\n' "$*" >>"$MOCK_CALLS"
if [[ "$#" -ne 6 || "$1" != "api" || "$2" != "--method" || "$3" != "GET" ||
    "$4" != /orgs/*/packages/container/* || "$5" != "--jq" || "$6" != ".visibility" ]]; then
    echo "Only explicit read-only package GET requests are supported" >&2
    exit 1
fi

case "$MOCK_SCENARIO" in
    denied)
        echo "gh: Forbidden (HTTP 403)" >&2
        exit 1
        ;;
    missing)
        echo "gh: Not Found (HTTP 404)" >&2
        exit 1
        ;;
    public | private | internal)
        echo "$MOCK_SCENARIO"
        ;;
    null | unknown | true)
        echo "$MOCK_SCENARIO"
        ;;
    empty)
        ;;
    object)
        echo '{"visibility":"public"}'
        ;;
    multiple)
        printf 'public\nprivate\n'
        ;;
    malformed)
        echo "gh: invalid JSON response" >&2
        exit 1
        ;;
    *)
        echo "Unknown mock scenario: $MOCK_SCENARIO" >&2
        exit 1
        ;;
esac
EOF
chmod +x "$mock_bin/gh"

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

assert_get() {
    local calls="$1"
    local endpoint="$2"
    [[ "$(cat "$calls")" == "api --method GET $endpoint --jq .visibility" ]] ||
        fail "expected exactly one read-only GET for $endpoint"
}

run_mock() {
    local scenario="$1"
    local calls="$2"
    shift 2

    env \
        PATH="$mock_bin:$PATH" \
        GH_TOKEN="test-token" \
        PACKAGE_VISIBILITY_ORG="${PACKAGE_VISIBILITY_ORG:-drasi-project}" \
        MOCK_SCENARIO="$scenario" \
        MOCK_CALLS="$calls" \
        "$visibility_script" "$@"
}

export PACKAGE_VISIBILITY_ORG=drasi-project
endpoint="/orgs/drasi-project/packages/container"

calls="$test_dir/missing-token-calls"
expect_failure env -u GH_TOKEN PATH="$mock_bin:$PATH" MOCK_CALLS="$calls" \
    "$visibility_script" verify-public drasi-plugin-directory
assert_contains "$output" "PACKAGES_ADMIN_TOKEN is not configured"
assert_contains "$output" "read:packages"
[[ ! -e "$calls" ]] || fail "missing token should fail before calling GitHub"

expect_failure env GH_TOKEN="" PATH="$mock_bin:$PATH" MOCK_CALLS="$calls" \
    "$visibility_script" verify-public drasi-plugin-directory
assert_contains "$output" "PACKAGES_ADMIN_TOKEN is not configured"
[[ ! -e "$calls" ]] || fail "empty token should fail before calling GitHub"

calls="$test_dir/public-calls"
output="$(run_mock public "$calls" verify-public drasi-plugin-directory)"
assert_contains "$output" "Verified drasi-project/drasi-plugin-directory is public (read-only check)"
assert_get "$calls" "$endpoint/drasi-plugin-directory"

for scenario in denied missing malformed; do
    calls="$test_dir/$scenario-calls"
    expect_failure run_mock "$scenario" "$calls" verify-public drasi-plugin-directory
    assert_contains "$output" "Unable to read package drasi-project/drasi-plugin-directory"
    assert_contains "$output" "package exists"
    assert_contains "$output" "read:packages"
    assert_contains "$output" "organization SSO"
    assert_get "$calls" "$endpoint/drasi-plugin-directory"
done

for scenario in private internal; do
    calls="$test_dir/$scenario-calls"
    expect_failure run_mock "$scenario" "$calls" verify-public source/http reaction/log
    assert_contains "$output" "is '$scenario', expected 'public'"
    assert_contains "$output" "GitHub Package settings"
    assert_contains "$output" "Change visibility to select Public, then rerun verification"
    assert_contains "$output" "does not change visibility"
    assert_get "$calls" "$endpoint/source%2Fhttp"
done

for scenario in null unknown true empty object multiple; do
    calls="$test_dir/$scenario-calls"
    expect_failure run_mock "$scenario" "$calls" verify-public source/http
    assert_contains "$output" "Unexpected visibility response"
    assert_get "$calls" "$endpoint/source%2Fhttp"
done

calls="$test_dir/invalid-calls"
for package in '' 'source/../invalid' '/source/http' 'source//http' 'source/http/' \
    'Source/http' 'source/http?visibility=public' 'source%2Fhttp' 'source/http log'; do
    expect_failure run_mock public "$calls" verify-public source/http "$package"
    assert_contains "$output" "Invalid package name"
    [[ ! -e "$calls" ]] || fail "all package names should be validated before calling GitHub"
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
[[ ! -e "$calls" ]] || fail "invalid arguments should fail before calling GitHub"

calls="$test_dir/packages-calls"
output="$(run_mock public "$calls" verify-public drasi-plugin-directory source/http index/rocksdb index/nested/plugin_v1.2)"
assert_contains "$output" "Verified drasi-project/index/rocksdb is public"
assert_contains "$output" "Verified drasi-project/index/nested/plugin_v1.2 is public"
expected="$(printf 'api --method GET %s/%s --jq .visibility\n' \
    "$endpoint" drasi-plugin-directory "$endpoint" source%2Fhttp \
    "$endpoint" index%2Frocksdb "$endpoint" index%2Fnested%2Fplugin_v1.2)"
[[ "$(cat "$calls")" == "$expected" ]] ||
    fail "all valid packages, including new categories and nested paths, should only be read"

calls="$test_dir/org-calls"
output="$(PACKAGE_VISIBILITY_ORG=other-org run_mock public "$calls" verify-public source/http)"
assert_contains "$output" "Verified other-org/source/http is public"
assert_get "$calls" "/orgs/other-org/packages/container/source%2Fhttp"

calls="$test_dir/invalid-org-calls"
PACKAGE_VISIBILITY_ORG=../invalid expect_failure run_mock public "$calls" verify-public source/http
assert_contains "$output" "Invalid organization name"
[[ ! -e "$calls" ]] || fail "invalid organization should fail before calling GitHub"

for method in PATCH POST PUT DELETE; do
    expect_failure env MOCK_CALLS="$test_dir/rejected-$method" MOCK_SCENARIO=public \
        "$mock_bin/gh" api --method "$method" "$endpoint/source%2Fhttp" --jq .visibility
    assert_contains "$output" "Only explicit read-only package GET requests are supported"
done

echo "package-visibility tests passed"
