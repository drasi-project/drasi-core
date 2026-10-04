#!/usr/bin/env bash
# Copyright 2026 The Drasi Authors.
# Licensed under the Apache License, Version 2.0.

set -euo pipefail
cd "$(dirname "$0")/../.."

coverage=false
if [[ "${1:-}" == --coverage ]]; then
    coverage=true
    shift
fi
if [[ $# -gt 1 ]]; then
    echo "Usage: bash lib/tests/run-computation-foundations.sh [--coverage] [report-directory]" >&2
    exit 2
fi
logs="${1:-target/foundation-qualification}"
mkdir -p "$logs"
printf 'incomplete\n' > "$logs/foundations.status"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-3}"
runner=(cargo test --locked)
if $coverage; then
    export CARGO_LLVM_COV_TARGET_DIR="${CARGO_LLVM_COV_TARGET_DIR:-$PWD/target/foundation-coverage}"
    export CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
    cargo llvm-cov --version > "$logs/coverage-tool-version.txt"
    # Only this coverage target is cleaned; native dependencies are retained.
    cargo llvm-cov clean --workspace
    runner=(cargo llvm-cov --no-report --locked)
fi

run() {
    local profile="$1"
    shift
    printf 'Running %s\n' "$profile"
    if "${runner[@]}" "$@" > "$logs/$profile.log" 2>&1; then
        grep '^test result:' "$logs/$profile.log"
    else
        cat "$logs/$profile.log" >&2
        printf 'failed: %s\n' "$profile" > "$logs/foundations.status"
        exit 1
    fi
}

# Both ordinary build profiles must discover and execute the added matrices.
run default -p drasi-lib --lib --tests
run no-default-features -p drasi-lib --no-default-features \
    --test computation_foundations --test computation_foundation_matrix \
    --test computation_contracts --test computation_deployment \
    --test computation_graph_dataflow --test computation_topology
run persistence -p drasi-lib --no-default-features --features computation-rocksdb-tests \
    --test computation_retained_pipes --test computation_transaction_transformer \
    --test computation_query_faults --test computation_query_recovery \
    --test computation_temporal_retractions --test computation_consumer_recovery
run integration -p lib-integration-tests \
    --test computation_qos --test computation_transaction_durability \
    --test computation_transaction_query --test computation_factory_lifecycle \
    --test computation_persistent_output --test computation_source_recovery \
    --test computation_consumer_contract --test computation_remote_effects \
    -- --test-threads=1

if $coverage; then
    cargo llvm-cov report --json --output-path "$logs/coverage.json"
    cargo llvm-cov report --lcov --output-path "$logs/coverage.lcov"
    cargo llvm-cov report --html --output-dir "$logs/coverage"
    python3 lib/tests/runtime_parity/foundation_coverage.py "$logs/coverage.json" \
        --output "$logs/foundation-coverage.json"
fi
printf 'passed\n' > "$logs/foundations.status"
echo "Foundation tests passed. This is not the whole-system replacement qualification gate."
