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

method="GET"
endpoint=""
while [[ "$#" -gt 0 ]]; do
    case "$1" in
        --method)
            method="$2"
            shift 2
            ;;
        -f | --jq)
            shift 2
            ;;
        /orgs/*)
            endpoint="$1"
            shift
            ;;
        *)
            shift
            ;;
    esac
done

printf '%s %s\n' "$method" "$endpoint" >>"$MOCK_CALLS"

case "$MOCK_SCENARIO" in
    denied)
        echo "gh: Not Found (HTTP 404)" >&2
        exit 1
        ;;
    public)
        echo "public"
        ;;
    private)
        if [[ "$method" == "PATCH" ]]; then
            echo "public"
        else
            echo "private"
        fi
        ;;
    unchanged)
        echo "private"
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

run_mock() {
    local scenario="$1"
    local calls="$2"
    shift 2

    env \
        PATH="$mock_bin:$PATH" \
        GH_TOKEN="test-token" \
        MOCK_SCENARIO="$scenario" \
        MOCK_CALLS="$calls" \
        "$visibility_script" "$@"
}

set +e
output="$(env -u GH_TOKEN "$visibility_script" check drasi-plugin-directory 2>&1)"
status="$?"
set -e
[[ "$status" -ne 0 ]] || fail "missing token should fail"
assert_contains "$output" "PACKAGES_ADMIN_TOKEN is not configured"

calls="$test_dir/check-calls"
output="$(run_mock public "$calls" check drasi-plugin-directory)"
assert_contains "$output" "access validated"
[[ "$(cat "$calls")" == "PATCH /orgs/drasi-project/packages/container/drasi-plugin-directory" ]] ||
    fail "preflight should perform one no-op visibility update"

calls="$test_dir/denied-calls"
set +e
output="$(run_mock denied "$calls" check drasi-plugin-directory 2>&1)"
status="$?"
set -e
[[ "$status" -ne 0 ]] || fail "denied preflight should fail"
assert_contains "$output" "cannot administer"

calls="$test_dir/public-calls"
output="$(run_mock public "$calls" set-public source/http)"
assert_contains "$output" "is already public"
[[ "$(wc -l <"$calls" | tr -d ' ')" -eq 1 ]] ||
    fail "an already-public package should not be updated"

calls="$test_dir/private-calls"
output="$(run_mock private "$calls" set-public source/http)"
assert_contains "$output" "Setting drasi-project/source/http to public"
[[ "$(cat "$calls")" == $'GET /orgs/drasi-project/packages/container/source%2Fhttp\nPATCH /orgs/drasi-project/packages/container/source%2Fhttp' ]] ||
    fail "a private package should be read and updated"

calls="$test_dir/unchanged-calls"
set +e
output="$(run_mock unchanged "$calls" set-public source/http 2>&1)"
status="$?"
set -e
[[ "$status" -ne 0 ]] || fail "an ineffective update should fail"
assert_contains "$output" "remained 'private'"

calls="$test_dir/invalid-calls"
set +e
output="$(run_mock public "$calls" set-public 'source/../invalid' 2>&1)"
status="$?"
set -e
[[ "$status" -ne 0 ]] || fail "an invalid package name should fail"
assert_contains "$output" "Invalid package name"
[[ ! -e "$calls" ]] || fail "an invalid package name should not call GitHub"

echo "package-visibility tests passed"
