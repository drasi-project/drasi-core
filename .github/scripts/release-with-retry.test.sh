#!/usr/bin/env bash

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
retry_script="$script_dir/release-with-retry.sh"
test_dir="$(mktemp -d)"
trap 'rm -rf "$test_dir"' EXIT

mock_release="$test_dir/mock-release.sh"
cat >"$mock_release" <<'EOF'
#!/usr/bin/env bash

count=0
if [[ -f "$MOCK_COUNT_FILE" ]]; then
    count="$(cat "$MOCK_COUNT_FILE")"
fi
count=$((count + 1))
printf '%s\n' "$count" >"$MOCK_COUNT_FILE"

case "$MOCK_SCENARIO" in
    success)
        exit 0
        ;;
    non-429)
        echo "authentication failed" >&2
        exit 42
        ;;
    foreign-429)
        echo "status 429 Too Many Requests from api.github.com" >&2
        exit 43
        ;;
    split-markers)
        echo "status 429 Too Many Requests from api.github.com" >&2
        echo "See https://crates.io/docs/rate-limits for unrelated documentation" >&2
        exit 44
        ;;
    retry-after)
        if [[ "$count" -eq 1 ]]; then
            echo "status 429 Too Many Requests: Please try again after Wed, 26 Aug 2026 01:02:32 GMT and see https://crates.io/docs/rate-limits" >&2
            exit 1
        fi
        exit 0
        ;;
    fallback)
        if [[ "$count" -le 2 ]]; then
            echo "status 429 Too Many Requests: https://crates.io/docs/rate-limits" >&2
            exit 1
        fi
        exit 0
        ;;
    always-429)
        echo "status 429 Too Many Requests: https://crates.io/docs/rate-limits" >&2
        exit 1
        ;;
esac
EOF
chmod +x "$mock_release"

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
    local count_file="$2"
    shift 2

    env \
        MOCK_SCENARIO="$scenario" \
        MOCK_COUNT_FILE="$count_file" \
        RELEASE_PUBLISH_DISABLE_SLEEP=true \
        "$@" \
        "$retry_script" "$mock_release"
}

count_file="$test_dir/success-count"
run_mock success "$count_file" >/dev/null
[[ "$(cat "$count_file")" -eq 1 ]] || fail "success should run once"

count_file="$test_dir/non-429-count"
set +e
output="$(run_mock non-429 "$count_file" 2>&1)"
status="$?"
set -e
[[ "$status" -eq 42 ]] || fail "non-429 status should be preserved"
[[ "$(cat "$count_file")" -eq 1 ]] || fail "non-429 failure should not retry"
assert_contains "$output" "not retrying"

count_file="$test_dir/foreign-429-count"
set +e
output="$(run_mock foreign-429 "$count_file" 2>&1)"
status="$?"
set -e
[[ "$status" -eq 43 ]] || fail "non-crates.io 429 status should be preserved"
[[ "$(cat "$count_file")" -eq 1 ]] || fail "non-crates.io 429 should not retry"
assert_contains "$output" "not retrying"

count_file="$test_dir/split-markers-count"
set +e
output="$(run_mock split-markers "$count_file" 2>&1)"
status="$?"
set -e
[[ "$status" -eq 44 ]] || fail "split-marker status should be preserved"
[[ "$(cat "$count_file")" -eq 1 ]] || fail "split markers should not retry"
assert_contains "$output" "not retrying"

count_file="$test_dir/retry-after-count"
output="$(
    run_mock retry-after "$count_file" \
        RELEASE_PUBLISH_NOW_EPOCH=1787706092 \
        RELEASE_PUBLISH_SAFETY_BUFFER_SECONDS=10 \
        RELEASE_PUBLISH_FALLBACK_BASE_SECONDS=999 \
        RELEASE_PUBLISH_FALLBACK_MAX_SECONDS=999
)"
[[ "$(cat "$count_file")" -eq 2 ]] || fail "retry-after should retry once"
assert_contains "$output" "retrying in 70s"

count_file="$test_dir/expired-retry-after-count"
output="$(
    run_mock retry-after "$count_file" \
        RELEASE_PUBLISH_NOW_EPOCH=1787706212 \
        RELEASE_PUBLISH_SAFETY_BUFFER_SECONDS=10
)"
[[ "$(cat "$count_file")" -eq 2 ]] || fail "expired retry-after should retry once"
assert_contains "$output" "retrying in 10s"

count_file="$test_dir/duration-count"
set +e
output="$(
    run_mock retry-after "$count_file" \
        RELEASE_PUBLISH_NOW_EPOCH=1787706092 \
        RELEASE_PUBLISH_MAX_DURATION_SECONDS=30 \
        RELEASE_PUBLISH_SAFETY_BUFFER_SECONDS=10 2>&1
)"
status="$?"
set -e
[[ "$status" -ne 0 ]] || fail "duration limit should fail"
[[ "$(cat "$count_file")" -eq 1 ]] || fail "duration limit should stop before retry"
assert_contains "$output" "would exceed"

count_file="$test_dir/exact-duration-count"
set +e
output="$(
    run_mock retry-after "$count_file" \
        RELEASE_PUBLISH_NOW_EPOCH=1787706092 \
        RELEASE_PUBLISH_MAX_DURATION_SECONDS=70 \
        RELEASE_PUBLISH_SAFETY_BUFFER_SECONDS=10 2>&1
)"
status="$?"
set -e
[[ "$status" -ne 0 ]] || fail "exact duration limit should fail"
[[ "$(cat "$count_file")" -eq 1 ]] || fail "exact duration limit should stop before retry"
assert_contains "$output" "would exceed"

count_file="$test_dir/fallback-count"
output="$(
    run_mock fallback "$count_file" \
        RELEASE_PUBLISH_FALLBACK_BASE_SECONDS=5 \
        RELEASE_PUBLISH_FALLBACK_MAX_SECONDS=20
)"
[[ "$(cat "$count_file")" -eq 3 ]] || fail "fallback should retry twice"
assert_contains "$output" "retrying in 5s"
assert_contains "$output" "retrying in 10s"

count_file="$test_dir/max-attempts-count"
set +e
output="$(
    run_mock always-429 "$count_file" \
        RELEASE_PUBLISH_MAX_ATTEMPTS=2 \
        RELEASE_PUBLISH_FALLBACK_BASE_SECONDS=1 2>&1
)"
status="$?"
set -e
[[ "$status" -ne 0 ]] || fail "attempt limit should fail"
[[ "$(cat "$count_file")" -eq 2 ]] || fail "attempt limit should stop at two"
assert_contains "$output" "retry limit reached"

echo "release-with-retry tests passed"
