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
    if [[ " $* " != *" --list "* ]]; then
        printf 'test result: ok. Synthetic runner self-check; no Rust tests executed.\n'
        return
    fi
    if [[ "${RUNNER_CHECK_OMIT:-}" == discovery && "$*" != *--no-default-features* ]]; then
        printf 'error: synthetic default discovery failure\n' >&2
        return 101
    fi
    for source in lib lib/tests/*.rs; do
        binary="${source##*/}"
        binary="${binary%.rs}"
        case "$binary" in
            computation_query_faults|computation_transaction_transformer|computation_pipeline_parity|computation_middleware_recovery)
                [[ "$*" == *computation-rocksdb-tests* ]] || continue
                ;;
        esac
        if [[ "${RUNNER_CHECK_OMIT:-}" == suite && "$binary" == computation_codec ]]; then
            continue
        fi
        if [[ "$binary" == lib ]]; then
            printf '     Running unittests src/lib.rs (target/debug/deps/drasi_lib-selfcheck)\n'
        else
            printf '     Running tests/%s.rs (target/debug/deps/%s-selfcheck)\n' "$binary" "$binary"
        fi
        awk -F '\t' -v binary="$binary" -v omit="${RUNNER_CHECK_OMIT:-}" '
            /^#/ || NF == 0 { next }
            FILENAME == ARGV[1] {
                mapped[$1 SUBSEP $2] = 1;
                if ($3 == binary) replacements[$4] = 1;
                next;
            }
            $1 == binary && !(($1 SUBSEP $2) in mapped) {
                if (omit == "baseline" && binary == "lib" && !omitted++) next;
                found[$2] = 1;
            }
            END {
                for (name in replacements) found[name] = 1;
                for (name in found) print name ": test";
            }
        ' lib/tests/runtime_parity/case-mappings.tsv lib/tests/runtime_parity/original-cases.tsv
        printf 'synthetic_runner_check: test\n'
    done
}
export -f cargo

bash lib/tests/run-runtime-parity.sh "$logs/valid" > "$logs/check.log" 2>&1
for omitted in baseline suite discovery; do
    if RUNNER_CHECK_OMIT="$omitted" bash lib/tests/run-runtime-parity.sh "$logs/missing-$omitted" \
        >> "$logs/check.log" 2>&1; then
        printf 'Runner accepted missing %s coverage.\n' "$omitted" >&2
        exit 1
    fi
    for profile in default no-default-features extra-capabilities; do
        test -f "$logs/missing-$omitted/$profile.exit-code"
    done
done
if DRASI_TEST_EXECUTION=component bash lib/tests/run-runtime-parity.sh "$logs/removed-selector" \
    >> "$logs/check.log" 2>&1; then
    printf 'Runner accepted the removed engine selector.\n' >&2
    exit 1
fi
printf 'Runner self-check passed: discovery, missing cases/suites, selector rejection, all profiles attempted.\n'
