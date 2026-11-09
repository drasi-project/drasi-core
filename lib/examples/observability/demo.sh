#!/usr/bin/env bash
# Copyright 2026 The Drasi Authors.
# Licensed under the Apache License, Version 2.0.
set -euo pipefail
export PYTHONDONTWRITEBYTECODE=1

if [[ "${1:-}" == --suite-smoke ]]; then
    [[ $# -eq 1 ]] || { printf '%s\n' '--suite-smoke takes no other arguments.' >&2; exit 2; }
    for variant in follow incoming-trace controls controls-never controls-no-traces controls-basic \
        controls-persistence controls-off controls-no-metrics controls-no-logs controls-excluded \
        latency latency-never latency-no-traces postgres; do
        scenario=controls; profile=debug; sampling=always; tracing=true; metrics=true; logs=info; exclusion=""; incoming=false
        case "$variant" in
            follow) scenario=follow ;;
            incoming-trace) scenario=follow; incoming=true ;;
            controls) ;;
            controls-never) sampling=never ;;
            controls-no-traces) tracing=false ;;
            controls-basic) profile=basic ;;
            controls-persistence) profile=persistence ;;
            controls-off) profile=off ;;
            controls-no-metrics) metrics=false ;;
            controls-no-logs) logs=off ;;
            controls-excluded) exclusion=drasi.query ;;
            latency) scenario=latency ;;
            latency-never) scenario=latency; sampling=never ;;
            latency-no-traces) scenario=latency; tracing=false ;;
            postgres) scenario=postgres ;;
        esac
        printf '\n=== %s ===\n' "$variant"
        DEMO_SCENARIO="$scenario" TELEMETRY_PROFILE="$profile" TRACE_SAMPLING="$sampling" \
            TRACE_ENABLED="$tracing" METRICS_ENABLED="$metrics" DEMO_LOG_LEVEL="$logs" \
            METRIC_EXCLUDE="$exclusion" DEMO_INCOMING_TRACE="$incoming" bash "$0" --smoke
    done
    exit 0
fi

smoke=0
scenario="${DEMO_SCENARIO:-follow}"
while [[ $# -gt 0 ]]; do
case "$1" in
    --smoke) smoke=1; shift ;;
    --scenario)
        [[ $# -ge 2 ]] || { printf '%s\n' '--scenario requires a value.' >&2; exit 2; }
        scenario="$2"; shift 2 ;;
    --help)
        printf 'Usage: bash %s [--smoke] [--scenario follow|controls|latency|postgres]\n       bash %s --suite-smoke\n' "$0" "$0"
        printf '%s\n' 'Start, seed, and verify the demo; Ctrl-C cleans up. --smoke exits after verification.'
        exit 0
        ;;
    *) printf 'Unknown argument: %s\n' "$1" >&2; exit 2 ;;
esac
done
case "$scenario" in follow|controls|latency|postgres) ;; *) printf '%s\n' 'Unknown demo scenario.' >&2; exit 2 ;; esac
export DEMO_SCENARIO="$scenario"

directory="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$directory/../../.." && pwd)"
engine="${CONTAINER_ENGINE:-podman}"
http_port="${HTTP_PORT:-19090}"
pg_port="${PGPORT:-15437}"
jaeger_port="${JAEGER_UI_PORT:-16689}"
otlp_port="${OTLP_HTTP_PORT:-14318}"
telemetry_port="${TELEMETRY_PORT:-19091}"
input_source=http
[[ "$scenario" != postgres ]] || input_source=postgres
postgres_name="drasi-otel-demo-postgres-$pg_port"
jaeger_name="drasi-otel-demo-jaeger-$jaeger_port"
export DEMO_POSTGRES_CONTAINER="$postgres_name" CONTAINER_ENGINE="$engine"
sampling="${TRACE_SAMPLING:-always}"
profile="${TELEMETRY_PROFILE:-debug}"
tracing="${TRACE_ENABLED:-true}"
incoming_trace="${DEMO_INCOMING_TRACE:-false}"
case "$incoming_trace" in true|false) ;; *) printf '%s\n' 'DEMO_INCOMING_TRACE must be true or false.' >&2; exit 2 ;; esac
case "$profile" in off|basic|debug|persistence) ;; *) printf '%s\n' 'Invalid TELEMETRY_PROFILE.' >&2; exit 2 ;; esac
case "$tracing" in true|false) ;; *) printf '%s\n' 'TRACE_ENABLED must be true or false.' >&2; exit 2 ;; esac
[[ "$profile" != off ]] || tracing=false
case "$sampling" in
    always|never) ;;
    *) printf '%s\n' 'TRACE_SAMPLING must be always or never.' >&2; exit 2 ;;
esac
if [[ "$incoming_trace" == true && ( "$scenario" != follow || "$tracing" != true || "$sampling" != always ) ]]; then
    printf '%s\n' 'The incoming-trace demonstration requires the follow scenario, tracing enabled, and TRACE_SAMPLING=always.' >&2
    exit 2
fi
# lib+cdylib dependencies emit unhashed artifacts; isolate this standalone
# workspace from the main workspace's plugin and test builds.
target="${CARGO_TARGET_DIR:-"$root/target"}/observability-demo"
case "$target" in /*) ;; *) target="$PWD/$target" ;; esac

commands=("$engine" cargo curl python3)
[[ "$scenario" != postgres ]] || commands+=(openssl)
for command in "${commands[@]}"; do
    command -v "$command" >/dev/null || {
        printf 'Required command is missing: %s\n' "$command" >&2
        exit 1
    }
done
"$engine" info >/dev/null
containers=("$jaeger_name")
[[ "$scenario" != postgres ]] || containers+=("$postgres_name")
for name in "${containers[@]}"; do
    if "$engine" container inspect "$name" >/dev/null 2>&1; then
        printf 'Container %s already exists; refusing to replace it.\n' "$name" >&2
        exit 1
    fi
done

printf '%s\n' 'Building the source-query-reaction demo (the first build can take a few minutes)...'
cargo build --manifest-path "$directory/Cargo.toml" --target-dir "$target" --locked --quiet

ports=("JAEGER_UI_PORT:$jaeger_port" "OTLP_HTTP_PORT:$otlp_port" "TELEMETRY_PORT:$telemetry_port")
if [[ "$scenario" == postgres ]]; then ports+=("PGPORT:$pg_port"); else ports+=("HTTP_PORT:$http_port"); fi
python3 - "${ports[@]}" <<'PY'
import socket
import sys

ports = []
for setting in sys.argv[1:]:
    name, raw = setting.split(":", 1)
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
    if [[ "$status" -ne 0 && -s "$log" ]]; then cat "$log" >&2; fi
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

password=""
if [[ "$scenario" == postgres ]]; then
printf '%s\n' 'Starting the optional PostgreSQL CDC database...'
password="$(openssl rand -hex 24)"
POSTGRES_PASSWORD="$password" "$engine" create --cidfile "$state/postgres.cid" \
    --name "$postgres_name" --env POSTGRES_PASSWORD \
    --publish "127.0.0.1:$pg_port:5432" docker.io/library/postgres:15-alpine \
    -c wal_level=logical -c max_replication_slots=10 -c max_wal_senders=10 >/dev/null
postgres="$(cat "$state/postgres.cid")"
"$engine" start "$postgres" >/dev/null
wait_for PostgreSQL "$engine" exec "$postgres" pg_isready -U postgres
"$engine" exec -i "$postgres" psql -U postgres -v ON_ERROR_STOP=1 >/dev/null <<'SQL'
CREATE TABLE trace_items (id INTEGER PRIMARY KEY, name TEXT NOT NULL, value INTEGER NOT NULL);
ALTER TABLE trace_items REPLICA IDENTITY FULL;
CREATE PUBLICATION drasi_trace_demo FOR TABLE trace_items;
SQL
fi
printf '%s\n' 'Starting Jaeger for the selected single-source demo...'
"$engine" create --cidfile "$state/jaeger.cid" --name "$jaeger_name" \
    --publish "127.0.0.1:$jaeger_port:16686" --publish "127.0.0.1:$otlp_port:4318" \
    cr.jaegertracing.io/jaegertracing/jaeger:2.22.0 >/dev/null
jaeger="$(cat "$state/jaeger.cid")"
"$engine" start "$jaeger" >/dev/null
jaeger_url="http://127.0.0.1:$jaeger_port"
wait_for Jaeger curl --fail --silent --show-error --max-time 2 "$jaeger_url/api/v3/services"

PGPASSWORD="$password" PGPORT="$pg_port" HTTP_PORT="$http_port" \
    TELEMETRY_PORT="$telemetry_port" TRACE_SAMPLING="$sampling" \
    JAEGER_URL="$jaeger_url" \
    OTEL_EXPORTER_OTLP_TRACES_ENDPOINT="http://127.0.0.1:$otlp_port/v1/traces" \
    "$target/debug/drasi-source-traces-example" >"$log" 2>&1 &
app_pid=$!
if [[ "$scenario" != postgres ]]; then
    wait_for 'HTTP source' curl --fail --silent --show-error --max-time 2 "http://127.0.0.1:$http_port/health"
fi
telemetry_url="http://127.0.0.1:$telemetry_port"
wait_for 'telemetry dashboard' curl --fail --silent --show-error --max-time 2 "$telemetry_url/health"
printf '\nDashboard ready: %s/\nMetrics: %s/metrics\nCorrelated logs: %s/logs\nJaeger: %s/\nService: drasi-observability-demo\n' \
    "$telemetry_url" "$telemetry_url" "$telemetry_url" "$jaeger_url"
printf '%s\n' 'Automatic checks are running. Wait for "Demo is live" before sending dashboard events.'
replication_active() {
    [[ "$("$engine" exec "$postgres" psql -U postgres -At -c \
        "SELECT active FROM pg_replication_slots WHERE slot_name='drasi_trace_demo';")" == t ]]
}
if [[ "$scenario" == postgres ]]; then
    wait_for 'active PostgreSQL replication' replication_active
fi
action() {
    curl --fail-with-body --silent --show-error --max-time 15 "$telemetry_url/action" \
        --header 'Content-Type: application/json' --data "{\"action\":\"$1\"}"
}
if [[ "$scenario" == latency ]]; then
    printf '%s\n' 'Preparing ten fan-out rows, then pausing only the slow handled sink...'
    action prepare
    action pause
fi

if [[ "$scenario" == postgres ]]; then
    printf '%s\n' 'Sending one live PostgreSQL insert (not a bootstrap snapshot)...'
    "$engine" exec "$postgres" psql -U postgres -v ON_ERROR_STOP=1 -c \
        "INSERT INTO trace_items VALUES (1, 'PostgreSQL demo', 11);"
else
    printf '%s\n' 'Sending one real HTTP event (no PostgreSQL source or database)...'
    seed_headers=(--header 'Content-Type: application/json')
    if [[ "$incoming_trace" == true ]]; then
        read -r incoming_id incoming_parent < <(python3 -c 'import secrets; print(secrets.token_hex(16), secrets.token_hex(8))')
        printf 'Upstream trace ID: %s\nUpstream parent span: %s\nHTTP header: traceparent: 00-%s-%s-01\n' \
            "$incoming_id" "$incoming_parent" "$incoming_id" "$incoming_parent"
        seed_headers+=(--header "traceparent: 00-$incoming_id-$incoming_parent-01" --header 'tracestate: demo=upstream')
    fi
    curl --fail-with-body --silent --show-error --max-time 10 \
        "http://127.0.0.1:$http_port/sources/http/events" "${seed_headers[@]}" \
        --data '{"operation":"insert","element":{"type":"node","id":"http-1","labels":["trace_items"],"properties":{"id":1,"name":"HTTP demo","value":7}}}'
    printf '\n'
fi
if [[ "$scenario" == latency ]]; then
    wait_for 'fast handling without slow completion' python3 "$directory/verify_scenarios.py" --url "$telemetry_url" --check paused-seed
    python3 "$directory/verify_scenarios.py" --url "$telemetry_url" --check paused-seed
    action release
fi

trace_id() {
    awk -v source="$1" '$1 == "RESULT" && $2 == "source=" source {
        for (i=1; i<=NF; i++) if ($i ~ /^trace_id=/) { sub(/^trace_id=/, "", $i); print $i; exit }
    }' "$log"
}
has_trace_id() { [[ -n "$(trace_id "$1")" ]]; }
for source in "$input_source"; do
    wait_for "$source callback" has_trace_id "$source"
    trace="$(trace_id "$source")"
    verification=("$source" "$trace" --jaeger "$jaeger_url" --profile "$profile" --scenario "$scenario")
    if [[ "$incoming_trace" == true ]]; then
        [[ "$trace" == "$incoming_id" ]] || { printf 'Incoming trace ID was not preserved: expected %s, got %s\n' "$incoming_id" "$trace" >&2; exit 1; }
        verification+=(--parent-span-id "$incoming_parent")
    fi
    if [[ "$sampling" == always && "$tracing" == true ]]; then
        wait_for "$source trace export" python3 "$directory/verify_trace.py" "${verification[@]}"
        python3 "$directory/verify_trace.py" "${verification[@]}"
    else
        printf 'NO EXPORT %s: context=%s tracing=%s sampling=%s; no Jaeger event trace is expected.\n' "$source" "$trace" "$tracing" "$sampling"
    fi
done
wait_for 'framework metrics and correlated handling logs' python3 "$directory/verify_telemetry.py" --url "$telemetry_url" --sampling "$sampling"
python3 "$directory/verify_telemetry.py" --url "$telemetry_url" --sampling "$sampling"
if [[ "$scenario" == postgres ]]; then
    action reset
    for statement in \
        "INSERT INTO trace_items VALUES (2, 'SQL demo', 7);" \
        "UPDATE trace_items SET value = 8 WHERE id = 2;" \
        "DELETE FROM trace_items WHERE id = 2;"; do
        "$engine" exec "$postgres" psql -U postgres -v ON_ERROR_STOP=1 -c "$statement" >/dev/null
    done
    wait_for 'SQL insert/update/delete handling' python3 "$directory/verify_scenarios.py" --url "$telemetry_url" --check actions
    python3 "$directory/verify_scenarios.py" --url "$telemetry_url" --check actions
    action reset
    printf '\nSQL input: %s exec -it %s psql -U postgres\n' "$engine" "$postgres_name"
else
    python3 "$directory/verify_scenarios.py" --url "$telemetry_url" --exercise
fi

printf '\nDashboard: %s\nMetrics: %s/metrics\nCorrelated logs: %s/logs\nJaeger: %s\nService: drasi-observability-demo\n' \
    "$telemetry_url" "$telemetry_url" "$telemetry_url" "$jaeger_url"
cat "$log"
if [[ "$smoke" -eq 1 ]]; then exit 0; fi
printf '\n%s\n' 'Demo is live. Open its trace links above. Press Ctrl-C to stop and clean up.'
if wait "$app_pid"; then
    app_pid=""
else
    status=$?
    app_pid=""
    cat "$log" >&2
    exit "$status"
fi
