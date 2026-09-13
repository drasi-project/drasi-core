#!/usr/bin/env bash
#
# Retry a release command only when crates.io returns HTTP 429.
# Usage: release-with-retry.sh <command> [args...]
#
# Runtime limits are configurable with RELEASE_PUBLISH_MAX_ATTEMPTS,
# RELEASE_PUBLISH_MAX_DURATION_SECONDS, RELEASE_PUBLISH_SAFETY_BUFFER_SECONDS,
# RELEASE_PUBLISH_FALLBACK_BASE_SECONDS, and RELEASE_PUBLISH_FALLBACK_MAX_SECONDS.
# RELEASE_PUBLISH_NOW_EPOCH and RELEASE_PUBLISH_DISABLE_SLEEP are test overrides.

set -euo pipefail

max_attempts="${RELEASE_PUBLISH_MAX_ATTEMPTS:-60}"
max_duration_seconds="${RELEASE_PUBLISH_MAX_DURATION_SECONDS:-7200}"
safety_buffer_seconds="${RELEASE_PUBLISH_SAFETY_BUFFER_SECONDS:-10}"
fallback_base_seconds="${RELEASE_PUBLISH_FALLBACK_BASE_SECONDS:-30}"
fallback_max_seconds="${RELEASE_PUBLISH_FALLBACK_MAX_SECONDS:-300}"

if [[ "$#" -eq 0 ]]; then
    echo "usage: $0 <command> [args...]" >&2
    exit 2
fi

for value in \
    "$max_attempts" \
    "$max_duration_seconds" \
    "$safety_buffer_seconds" \
    "$fallback_base_seconds" \
    "$fallback_max_seconds"; do
    if ! [[ "$value" =~ ^[0-9]+$ ]]; then
        echo "retry settings must be non-negative integers" >&2
        exit 2
    fi
done

if ((
    max_attempts < 1 ||
        max_duration_seconds < 1 ||
        fallback_base_seconds < 1 ||
        fallback_max_seconds < fallback_base_seconds
)); then
    echo "max attempts, duration, and fallback delays must be valid positive values" >&2
    exit 2
fi

work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir"' EXIT

current_epoch() {
    if [[ -n "${RELEASE_PUBLISH_NOW_EPOCH:-}" ]]; then
        printf '%s\n' "$RELEASE_PUBLISH_NOW_EPOCH"
    else
        date +%s
    fi
}

parse_retry_epoch() {
    local log_file="$1"
    local retry_at

    retry_at="$(
        sed -n 's/^.*Please try again after \(.* GMT\) and see.*$/\1/p' "$log_file" |
            tail -n 1
    )"
    if [[ -z "$retry_at" ]]; then
        return 1
    fi

    date -u -d "$retry_at" +%s 2>/dev/null ||
        date -j -u -f '%a, %d %b %Y %H:%M:%S %Z' "$retry_at" +%s 2>/dev/null
}

is_crates_io_rate_limit() {
    local log_file="$1"
    grep -qE '429 Too Many Requests.*https://crates\.io/docs/rate-limits' "$log_file"
}

fallback_delay() {
    local attempt="$1"
    local exponent=$((attempt - 1))
    local delay="$fallback_base_seconds"

    while ((exponent > 0 && delay < fallback_max_seconds)); do
        delay=$((delay * 2))
        exponent=$((exponent - 1))
    done

    if ((delay > fallback_max_seconds)); then
        delay="$fallback_max_seconds"
    fi
    printf '%s\n' "$delay"
}

sleep_for() {
    local delay="$1"
    if [[ "${RELEASE_PUBLISH_DISABLE_SLEEP:-false}" != "true" ]]; then
        sleep "$delay"
    fi
}

started_at="$(current_epoch)"
attempt=1

while true; do
    attempt_log="$work_dir/attempt-$attempt.log"
    echo "Release publish attempt $attempt of $max_attempts"

    set +e
    "$@" 2>&1 | tee "$attempt_log"
    status="${PIPESTATUS[0]}"
    set -e

    if [[ "$status" -eq 0 ]]; then
        exit 0
    fi

    if ! is_crates_io_rate_limit "$attempt_log"; then
        echo "Release failed with a non-rate-limit error; not retrying." >&2
        exit "$status"
    fi

    now="$(current_epoch)"
    elapsed=$((now - started_at))
    if ((attempt >= max_attempts || elapsed >= max_duration_seconds)); then
        echo "Release retry limit reached after $attempt attempts and ${elapsed}s." >&2
        exit "$status"
    fi

    if retry_epoch="$(parse_retry_epoch "$attempt_log")"; then
        delay=$((retry_epoch - now + safety_buffer_seconds))
        if ((delay < safety_buffer_seconds)); then
            delay="$safety_buffer_seconds"
        fi
    else
        delay="$(fallback_delay "$attempt")"
    fi

    if ((elapsed + delay >= max_duration_seconds)); then
        echo "Release retry would exceed the ${max_duration_seconds}s duration limit." >&2
        exit "$status"
    fi

    echo "Crates.io rate limit reached; retrying in ${delay}s."
    sleep_for "$delay"
    attempt=$((attempt + 1))
done
