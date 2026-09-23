#!/usr/bin/env bash
# Copyright 2026 The Drasi Authors.
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

# Validate discovery and failure reporting with a shell-local Cargo stub.
# This checks the runner itself; it does not compile or execute Rust tests.
set -euo pipefail
cd "$(dirname "$0")/../../.." || exit 1
logs="${1:-target/runtime-runner-self-check}"
mkdir -p "$logs"

cargo() {
    local listing=false
    [[ " $* " != *" --list "* ]] || listing=true
    if [[ "$listing" == true && "${RUNNER_CHECK_OMIT:-}" == discovery && "$*" != *--no-default-features* ]]; then
        printf 'error: synthetic default discovery failure\n' >&2
        return 101
    fi
    local inventory=lib/tests/runtime_parity/original-cases.tsv
    local mappings=lib/tests/runtime_parity/case-mappings.tsv
    local package=drasi-lib profile=default
    [[ "$*" != *--no-default-features* ]] || profile=no-default-features
    [[ "$*" != *computation-rocksdb-tests* ]] || profile=extra-capabilities
    local sources=(lib lib/tests/*.rs)
    if [[ " $* " == *" -p lib-integration-tests "* ]]; then
        inventory=lib/tests/runtime_parity/integration-cases.tsv
        mappings=/dev/null
        package=lib-integration-tests
        profile=integration
        sources=(lib-integration-tests/tests/*.rs)
    elif [[ " $* " == *" -p drasi-plugin-sdk "* ]]; then
        inventory=/dev/null
        mappings=/dev/null
        package=drasi-plugin-sdk
        profile=plugin-factories
        sources=(components/plugin-sdk/tests/computation_factories.rs)
    fi
    local source binary
    for source in "${sources[@]}"; do
        binary="${source##*/}"
        binary="${binary%.rs}"
        case "$binary" in
            computation_query_faults|computation_transaction_transformer|computation_pipeline_parity|computation_middleware_recovery)
                [[ "$*" == *computation-rocksdb-tests* ]] || continue
                ;;
        esac
        if [[ "${RUNNER_CHECK_OMIT:-}" == suite && "$binary" == computation_codec && "$listing" == true ]]; then
            continue
        fi
        if [[ "$binary" == lib ]]; then
            printf '     Running unittests src/lib.rs (target/debug/deps/drasi_lib-selfcheck)\n'
        else
            printf '     Running tests/%s.rs (target/debug/deps/%s-selfcheck)\n' "$binary" "$binary"
        fi
        awk -F '\t' -v binary="$binary" -v omit="${RUNNER_CHECK_OMIT:-}" -v listing="$listing" \
            -v package="$package" -v profile="$profile" '
            /^#/ || NF == 0 { next }
            FILENAME == ARGV[1] {
                mapped[$1 SUBSEP $2] = 1;
                if ($3 == binary) replacements[$4] = 1;
                next;
            }
            FILENAME == ARGV[2] {
                if ($1 == binary && !(($1 SUBSEP $2) in mapped)) {
                    if (listing == "true" && omit == "baseline" && binary == "lib" && !omitted++) next;
                    found[$2] = 1;
                }
                next;
            }
            FILENAME == ARGV[3] {
                if ($1 == package && $2 == binary && ($4 == "all" || $4 == profile)) found[$3] = 1;
                next;
            }
            FILENAME == ARGV[4] {
                if ($1 == package && $2 == binary) {
                    found[$3] = 1;
                    ignored[$3] = 1;
                    drivers[$4] = 1;
                }
            }
            END {
                for (name in replacements) found[name] = 1;
                for (name in found) {
                    if (omit == "capability" && name == "imported_external_components_must_be_supplied_and_execute_in_a_fresh_graph") continue;
                    if (omit == "mapping" && name == "full_pipeline_preserves_main_ownership_subscription_and_removal_results") continue;
                    if (listing == "true") {
                        print name ": test";
                    } else if (omit != "execution" || binary != "lib" || omitted++) {
                        outcome = name in ignored ? "ignored, approved" : "ok";
                        if (omit == "ignored" && binary == "lib") outcome = "ignored, injected";
                        if (omit == "worker-driver" && name in drivers) outcome = "ignored, injected";
                        print "test " name " ... " outcome;
                    }
                }
            }
        ' "$mappings" "$inventory" lib/tests/runtime_parity/computation-contracts.tsv \
            lib/tests/runtime_parity/allowed-ignored.tsv
        if [[ "$listing" == true ]]; then
            printf 'synthetic_runner_check: test\n'
        elif [[ "${RUNNER_CHECK_STYLE:-}" == interleaved ]]; then
            printf 'test synthetic_runner_check - should panic ... \033[32m2026-09-22 INFO concurrent log\033[0m\nanother log\nok\033[32m2026-09-22 INFO more logging\033[0m\n'
        elif [[ "${RUNNER_CHECK_OMIT:-}" == lost-result ]]; then
            printf 'test synthetic_runner_check ... concurrent log without a result\n'
        else
            printf 'test synthetic_runner_check ... ok\n'
        fi
    done
    if [[ "$listing" == false ]]; then
        printf 'test result: ok. Synthetic runner self-check; no Rust tests executed.\n'
        if [[ "${RUNNER_CHECK_OMIT:-}" == failure ]]; then return 101; fi
    fi
    return 0
}
export -f cargo

bash lib/tests/run-runtime-parity.sh "$logs/valid" > "$logs/check.log" 2>&1
RUNNER_CHECK_STYLE=interleaved bash lib/tests/run-runtime-parity.sh "$logs/interleaved" \
    >> "$logs/check.log" 2>&1
for omitted in baseline suite discovery execution ignored failure capability mapping worker-driver lost-result; do
    if RUNNER_CHECK_OMIT="$omitted" bash lib/tests/run-runtime-parity.sh "$logs/missing-$omitted" \
        >> "$logs/check.log" 2>&1; then
        printf 'Runner accepted missing %s coverage.\n' "$omitted" >&2
        exit 1
    fi
    for profile in default no-default-features extra-capabilities integration plugin-factories; do
        test -f "$logs/missing-$omitted/$profile.exit-code"
    done
done
if DRASI_TEST_EXECUTION=component bash lib/tests/run-runtime-parity.sh "$logs/removed-selector" \
    >> "$logs/check.log" 2>&1; then
    printf 'Runner accepted the removed engine selector.\n' >&2
    exit 1
fi
printf 'Runner self-check passed: missing discovery/cases/suites/execution, ignored regressions, failed execution, selector rejection, all profiles attempted.\n'
