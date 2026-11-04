#!/usr/bin/env bash
# Copyright 2026 The Drasi Authors.
# Licensed under the Apache License, Version 2.0.

# Select one development gate, not the complete runtime-conformance matrix.
set -euo pipefail

usage() {
    cat <<'USAGE'
Usage: bash lib/tests/run-query-plugin.sh [smoke|ordinary|non-atomic|retained-recovery|shared-query|subscriptions|evaluator|scheduling|archives|middleware|joins|bootstrap|provider|sqlite-live|durable-bootstrap|durability|native-recovery|progress|boundary|semantics|recovery|timers|api]

  smoke       Build query, SQLite and middleware libraries; run all loaded-query tests (default).
  ordinary    Required factory registration, loaded ordinary queries, large reads and test clocks.
  non-atomic  Shared publication protocol, independent persistent writers, reset/crash and timers.
  retained-recovery Canonical metrics, retained native reads, completed cleanup and consumer policies.
  shared-query Test shared result ownership, retained history/follow and ordinary native-query reads.
  subscriptions Test ordinary channel/broadcast readers and reactions against the loaded query.
  evaluator   Test retained Core evaluator setup, scoped indexes, bootstrap/live reuse and restart.
  scheduling  Test Core future processing through native wakeups, continuation and restart.
  archives    Test point/range historical queries against actual native archive providers.
  middleware  Test Core input middleware, aliases, bootstrap origins and custom factory lifetime.
  joins       Test shared join settings, built-in parity, SQLite bootstrap/live joins and recovery.
  bootstrap   Test volatile snapshots, two-input joins, and live handover.
  provider    Test existing file-provider integration, live capture, and handover guards.
  sqlite-live Test real SQLite bootstrap/live writes and interrupted-startup restart.
  durable-bootstrap Test indexed file/bootstrap state and coordinated SQLite handover.
  durability  Test atomic state/snapshots, reopen, process exit, and graph replay.
  native-recovery Test loaded-query delivery, fanout/shared commits, and deletion.
  progress    Test recovered/committed input views and publication/lifecycle fencing.
  boundary    Rebuild native fixtures; test indexed processing and result-key transport.
  semantics   Existing query, configuration, and codec regression targets.
  recovery    Existing RocksDB query faults, bootstrap/recovery, and source transactions.
  timers      Existing deterministic clock and temporal-retraction regression targets.
  api         Existing query input-port, public API, and result-subscription targets.

The semantics/recovery/timers groups and api's library tests explicitly register
the linked reference transformer. They are preservation baselines, NOT evidence
of native execution. api's integration subscription cases register the native
query factory. No group runs Server tests or the full workspace suite.

Cargo environment settings are inherited. Run groups serially when sharing a
target directory. Both plugin and host must be built for the local host target.
USAGE
}

if [[ $# -gt 1 ]]; then
    usage >&2
    exit 2
fi

group="${1:-smoke}"
case "$group" in
    -h|--help)
        usage
        exit 0
        ;;
    smoke|ordinary|non-atomic|retained-recovery|shared-query|subscriptions|evaluator|scheduling|archives|middleware|joins|bootstrap|provider|sqlite-live|durable-bootstrap|durability|native-recovery|progress|boundary|semantics|recovery|timers|api) ;;
    *)
        printf 'Unknown query-plugin test group: %s\n' "$group" >&2
        usage >&2
        exit 2
        ;;
esac

cd "$(dirname "${BASH_SOURCE[0]}")/../.."
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-1}"
# The test binary loads the cdylib from this exact target directory.
target_directory="$(cargo metadata --no-deps --format-version 1 |
    python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')"
export CARGO_TARGET_DIR="$target_directory"

build_query_plugin() {
    case "$group" in
        smoke|durability|boundary|middleware)
            cargo build -p drasi-host-sdk --example native_custom_query
            ;;
    esac
    cargo build -p drasi-computation-transformer-query -p drasi-computation-bootstrap-sqlite \
        -p drasi-computation-source-sqlite -p drasi-computation-transformer-middleware \
        --lib --features dynamic-plugin
}

case "$group" in
    smoke)
        build_query_plugin
        cargo test -p drasi-host-sdk --features test-support --test native_query_plugin -- --test-threads=1
        ;;
    ordinary)
        cargo test -p drasi-lib --test query_factory_registration -- --test-threads=1
        build_query_plugin
        cargo test -p drasi-host-sdk --features test-support --test native_query_plugin durable::ordinary:: -- --test-threads=1
        ;;
    non-atomic)
        cargo test -p drasi-query-common --lib publication::
        build_query_plugin
        cargo test -p drasi-host-sdk --features test-support --lib --test native_query_plugin non_atomic -- --test-threads=1
        ;;
    retained-recovery)
        cargo test -p drasi-lib --lib query_api::tests:: -- --test-threads=1
        cargo test -p drasi-lib --test computation_consumer_recovery -- --test-threads=1
        build_query_plugin
        cargo test -p drasi-host-sdk --features test-support --lib --test native_query_plugin -- \
            retained_recovery:: corrupt_recovery:: reset_baselines_ \
            stop_releases_an_abandoned_transfer --test-threads=1
        ;;
    shared-query)
        cargo test -p drasi-query-common --doc
        cargo test -p drasi-lib --lib query::fingerprint_tests:: -- --test-threads=1
        build_query_plugin
        cargo test -p drasi-host-sdk --test native_query_plugin -- \
            shared_query_output_ shared_query_definition_ native_query_uses_ordinary_library_read_api \
            --test-threads=1
        ;;
    subscriptions)
        cargo test -p drasi-lib --lib query_catalog::tests:: -- --test-threads=1
        cargo test -p lib-integration-tests --test query_result_subscriptions -- --test-threads=1
        build_query_plugin
        cargo test -p drasi-host-sdk --lib --test native_query_plugin -- \
            computation::output_reads::tests:: native_query_subscriptions_ \
            --test-threads=1
        ;;
    evaluator)
        cargo test -p drasi-computation-transformer-query --lib
        build_query_plugin
        cargo test -p drasi-host-sdk --test native_query_plugin retained_evaluator -- --test-threads=1
        ;;
    scheduling)
        build_query_plugin
        cargo test -p drasi-host-sdk --test native_query_plugin shared_query_scheduling_ -- --test-threads=1
        cargo test -p drasi-lib --features computation-rocksdb-tests \
            --test computation_temporal_retractions -- --test-threads=1
        ;;
    archives)
        build_query_plugin
        cargo test -p drasi-host-sdk --test native_query_plugin shared_query_archives_ -- --test-threads=1
        ;;
    middleware)
        build_query_plugin
        cargo test -p drasi-host-sdk --test native_query_plugin shared_query_middleware_ -- --test-threads=1
        ;;
    joins)
        cargo test -p drasi-computation-contracts --lib query_joins::
        cargo test -p drasi-lib --lib query::fingerprint_tests:: -- --test-threads=1
        build_query_plugin
        cargo test -p drasi-host-sdk --test native_query_plugin synthetic_join -- --test-threads=1
        ;;
    bootstrap)
        build_query_plugin
        cargo test -p drasi-host-sdk --test native_query_plugin bootstrap:: -- --test-threads=1
        ;;
    provider)
        build_query_plugin
        cargo test -p drasi-host-sdk --test native_query_plugin bootstrap::provider:: -- --test-threads=1
        ;;
    sqlite-live)
        build_query_plugin
        cargo test -p drasi-host-sdk --test native_query_plugin bootstrap::provider::sqlite_live:: -- --test-threads=1
        ;;
    durability)
        build_query_plugin
        cargo test -p drasi-host-sdk --test native_query_plugin durable:: -- --test-threads=1
        cargo test -p drasi-host-sdk --lib -- \
            native_indexed_processing_commits_rolls_back_and_revokes \
            native_indexed_processing_rejects_atomic_state \
            native_indexed_processing_trims_large_output_after_projection \
            native_indexed_processing_progress_ \
            --test-threads=1
        ;;
    durable-bootstrap)
        build_query_plugin
        cargo test -p drasi-host-sdk --test native_query_plugin durable::bootstrap:: -- --test-threads=1
        cargo test -p drasi-host-sdk --lib native_indexed_processing_bootstrap_ -- --test-threads=1
        ;;
    progress)
        build_query_plugin
        cargo test -p drasi-host-sdk --test native_query_plugin durable::progress:: -- --test-threads=1
        cargo test -p drasi-host-sdk --lib native_indexed_processing_progress_ -- --test-threads=1
        ;;
    native-recovery)
        build_query_plugin
        cargo test -p drasi-host-sdk --test native_query_plugin -- \
            durable::destinations:: \
            indexed_deprovision_ \
            indexed_preparation_ \
            indexed_state_progress_and_pending_output_ \
            process_exit_replays_committed_output_through_graph \
            --test-threads=1
        ;;
    boundary)
        build_query_plugin
        cargo build -p drasi-host-sdk --example native_recovery
        cargo test -p drasi-core-types --lib interface::result_index::tests:: -- --test-threads=1
        cargo test -p drasi-host-sdk --lib native_indexed_processing -- --test-threads=1
        cargo test -p drasi-host-sdk --test native_query_plugin -- --test-threads=1
        ;;
    semantics)
        printf 'Preservation baseline: these targets explicitly register the linked reference query.\n'
        cargo test -p drasi-lib \
            --test computation_queries \
            --test computation_query_configuration \
            --test computation_query_codec \
            -- --test-threads=1
        ;;
    recovery)
        printf 'Preservation baseline: these targets explicitly register the linked reference query.\n'
        cargo test -p drasi-lib --features computation-rocksdb-tests \
            --test computation_query_faults \
            --test computation_query_recovery \
            --test computation_source_transactions \
            -- --test-threads=1
        ;;
    timers)
        printf 'Preservation baseline: these targets explicitly register the linked reference query.\n'
        cargo test -p drasi-lib --features computation-rocksdb-tests,test-support \
            --test query_test_control \
            --test computation_temporal_retractions \
            -- --test-threads=1
        ;;
    api)
        printf 'Library baseline: explicitly registered reference query; subscription integration uses the native factory.\n'
        cargo test -p drasi-lib \
            --test computation_query_input_ports \
            --test component_api_parity \
            -- --test-threads=1
        cargo test -p lib-integration-tests --test query_result_subscriptions -- --test-threads=1
        ;;
esac
