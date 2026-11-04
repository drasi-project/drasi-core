#!/usr/bin/env bash
# Copyright 2026 The Drasi Authors.
# Licensed under the Apache License, Version 2.0.
set -euo pipefail

smoke=0
case "${1:-}" in
    "") ;;
    --smoke) smoke=1 ;;
    --help)
        printf 'Usage: bash %s [--smoke]\n' "$0"
        printf '%s\n' 'Start, seed, and verify the demo; Ctrl-C cleans up. --smoke exits after verification.'
        exit 0
        ;;
    *) printf 'Unknown argument: %s\n' "$1" >&2; exit 2 ;;
esac
if [[ $# -gt 1 ]]; then
    printf '%s\n' 'Expected at most one argument: --smoke' >&2
    exit 2
fi

directory="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$directory/../../.." && pwd)"
engine="${CONTAINER_ENGINE:-podman}"
http_port="${HTTP_PORT:-19090}"
pg_port="${PGPORT:-15437}"
jaeger_port="${JAEGER_UI_PORT:-16689}"
otlp_port="${OTLP_HTTP_PORT:-14318}"
target="${CARGO_TARGET_DIR:-"$root/target"}"
case "$target" in /*) ;; *) target="$PWD/$target" ;; esac

for command in "$engine" cargo curl openssl python3; do
    command -v "$command" >/dev/null || {
        printf 'Required command is missing: %s\n' "$command" >&2
        exit 1
    }
done
"$engine" info >/dev/null
for name in drasi-otel-demo-postgres drasi-otel-demo-jaeger; do
    if "$engine" container inspect "$name" >/dev/null 2>&1; then
        printf 'Container %s already exists; refusing to replace it.\n' "$name" >&2
        exit 1
    fi
done

printf '%s\n' 'Building the demo (the first build can take a few minutes)...'
cargo build --manifest-path "$directory/Cargo.toml" --target-dir "$target" --locked --quiet

python3 - "$http_port" "$pg_port" "$jaeger_port" "$otlp_port" <<'PY'
import socket
import sys

names = ["HTTP_PORT", "PGPORT", "JAEGER_UI_PORT", "OTLP_HTTP_PORT"]
ports = []
for name, raw in zip(names, sys.argv[1:]):
    try:
        port = int(raw)
        if not 1 <= port <= 65535:
            raise ValueError()
    except ValueError:
        raise SystemExit(f"{name} must be a TCP port from 1 to 65535")
    if port in ports:
        raise SystemExit("Demo ports must be distinct")
    ports.append(port)
    with socket.socket() as listener:
        listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        try:
            listener.bind(("127.0.0.1", port))
        except OSError as error:
            raise SystemExit(f"{name}={port} is unavailable: {error}")
PY

state="$(mktemp -d "${TMPDIR:-/tmp}/drasi-observability.XXXXXX")"
log="$state/application.log"
app_pid=""
cleanup() {
    local status=$?
    local cid file child_status
    local cleanup_failed=0
    trap - EXIT
    trap '' INT TERM
    if [[ -n "$app_pid" ]]; then
        if kill -0 "$app_pid" 2>/dev/null; then
            if ! kill -INT "$app_pid" 2>/dev/null; then
                printf '%s\n' 'Application exited before the stop signal; collecting its status.' >&2
            fi
        fi
        if wait "$app_pid"; then
            :
        else
            child_status=$?
            printf 'Demo application exited with status %s.\n' "$child_status" >&2
            if [[ -s "$log" ]]; then cat "$log" >&2; fi
            [[ "$status" -ne 0 ]] || status="$child_status"
        fi
    fi
    for file in "$state/postgres.cid" "$state/jaeger.cid"; do
        if [[ -s "$file" ]]; then
            cid="$(cat "$file")"
            if [[ "$cid" =~ ^[0-9a-f]{12,64}$ ]]; then
                if ! "$engine" rm --force --volumes "$cid" >/dev/null; then
                    printf 'Could not remove owned demo container %s.\n' "$cid" >&2
                    cleanup_failed=1
                fi
            else
                printf 'Invalid owned container ID in %s; refusing removal.\n' "$file" >&2
                cleanup_failed=1
            fi
        fi
    done
    if [[ "$cleanup_failed" -eq 1 ]]; then
        printf 'Cleanup incomplete; owned container IDs and logs retained in %s.\n' "$state" >&2
        status=1
    elif rm -f -- "$log" "$state/postgres.cid" "$state/jaeger.cid" && rmdir "$state"; then
        printf '%s\n' 'Demo application, containers, and anonymous volumes cleaned up.'
    else
        printf 'Could not remove temporary demo files in %s.\n' "$state" >&2
        status=1
    fi
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

fail() {
    printf '%s\n' "$1" >&2
    if [[ -s "$log" ]]; then cat "$log" >&2; fi
    exit 1
}

wait_for() {
    local description="$1"
    local attempt
    shift
    for ((attempt=0; attempt<60; attempt+=1)); do
        if [[ -n "$app_pid" ]] && ! kill -0 "$app_pid" 2>/dev/null; then
            fail 'Demo application stopped unexpectedly.'
        fi
        if "$@" >/dev/null 2>&1; then return; fi
        sleep 1
    done
    if ! "$@"; then
        printf 'Last readiness check failed: %s\n' "$description" >&2
    fi
    fail "Timed out waiting for $description."
}

printf '%s\n' 'Starting isolated PostgreSQL and Jaeger containers...'
password="$(openssl rand -hex 24)"
POSTGRES_PASSWORD="$password" "$engine" create --cidfile "$state/postgres.cid" \
    --name drasi-otel-demo-postgres --env POSTGRES_PASSWORD \
    --publish "127.0.0.1:$pg_port:5432" docker.io/library/postgres:15-alpine \
    -c wal_level=logical -c max_replication_slots=10 -c max_wal_senders=10 >/dev/null
postgres="$(cat "$state/postgres.cid")"
"$engine" start "$postgres" >/dev/null
"$engine" create --cidfile "$state/jaeger.cid" --name drasi-otel-demo-jaeger \
    --publish "127.0.0.1:$jaeger_port:16686" --publish "127.0.0.1:$otlp_port:4318" \
    cr.jaegertracing.io/jaegertracing/jaeger:2.22.0 >/dev/null
jaeger="$(cat "$state/jaeger.cid")"
"$engine" start "$jaeger" >/dev/null
jaeger_url="http://127.0.0.1:$jaeger_port"
wait_for PostgreSQL "$engine" exec "$postgres" pg_isready -U postgres
wait_for Jaeger curl --fail --silent --show-error --max-time 2 "$jaeger_url/api/v3/services"
"$engine" exec -i "$postgres" psql -U postgres -v ON_ERROR_STOP=1 >/dev/null <<'SQL'
CREATE TABLE trace_items (id INTEGER PRIMARY KEY, name TEXT NOT NULL, value INTEGER NOT NULL);
ALTER TABLE trace_items REPLICA IDENTITY FULL;
CREATE PUBLICATION drasi_trace_demo FOR TABLE trace_items;
SQL

PGPASSWORD="$password" PGPORT="$pg_port" HTTP_PORT="$http_port" \
    OTEL_EXPORTER_OTLP_TRACES_ENDPOINT="http://127.0.0.1:$otlp_port/v1/traces" \
    "$target/debug/drasi-source-traces-example" >"$log" 2>&1 &
app_pid=$!
wait_for 'HTTP source' curl --fail --silent --show-error --max-time 2 "http://127.0.0.1:$http_port/health"
replication_active() {
    [[ "$("$engine" exec "$postgres" psql -U postgres -At -c \
        "SELECT active FROM pg_replication_slots WHERE slot_name='drasi_trace_demo';")" == t ]]
}
wait_for 'active PostgreSQL replication' replication_active

printf '%s\n' 'Sending one real HTTP event and one PostgreSQL insert...'
curl --fail-with-body --silent --show-error --max-time 10 \
    "http://127.0.0.1:$http_port/sources/http/events" --header 'Content-Type: application/json' \
    --data '{"operation":"insert","element":{"type":"node","id":"http-1","labels":["trace_items"],"properties":{"id":1,"name":"HTTP demo","value":7}}}'
printf '\n'
"$engine" exec "$postgres" psql -U postgres -v ON_ERROR_STOP=1 -c \
    "INSERT INTO trace_items VALUES (1, 'PostgreSQL demo', 11);"

trace_id() {
    awk -v source="$1" '$1 == "RESULT" && $2 == "source=" source {
        for (i=1; i<=NF; i++) if ($i ~ /^trace_id=/) { sub(/^trace_id=/, "", $i); print $i; exit }
    }' "$log"
}
has_trace_id() { [[ -n "$(trace_id "$1")" ]]; }
for source in http postgres; do
    wait_for "$source callback" has_trace_id "$source"
    trace="$(trace_id "$source")"
    wait_for "$source trace export" python3 "$directory/verify_trace.py" "$source" "$trace" --jaeger "$jaeger_url"
    python3 "$directory/verify_trace.py" "$source" "$trace" --jaeger "$jaeger_url"
done

printf '\nJaeger: %s\nService: drasi-source-traces\n' "$jaeger_url"
cat "$log"
if [[ "$smoke" -eq 1 ]]; then exit 0; fi
printf '\n%s\n' 'Demo is live. Open either trace URL above. Press Ctrl-C to stop and clean up.'
if wait "$app_pid"; then
    app_pid=""
else
    status=$?
    app_pid=""
    cat "$log" >&2
    exit "$status"
fi
