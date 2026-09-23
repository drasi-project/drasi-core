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

set -uo pipefail

cd "$(dirname "$0")/../.." || exit 1
logs="${1:-target/runtime-conformance}"
export CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=3
export TMPDIR="$PWD/target/runtime-conformance-scratch"
unset RUST_LOG
if [[ "${DRASI_TEST_EXECUTION+x}" == x && "$DRASI_TEST_EXECUTION" != computation ]]; then
    printf 'Unsupported DRASI_TEST_EXECUTION=%s: ComputationGraph is the sole runtime.\n' \
        "$DRASI_TEST_EXECUTION" >&2
    exit 1
fi
unset DRASI_TEST_EXECUTION
mkdir -p "$logs" "$TMPDIR" || exit 1
failed=0

for profile in default no-default-features extra-capabilities integration plugin-factories; do
    package=drasi-lib
    inventory=lib/tests/runtime_parity/original-cases.tsv
    mappings=lib/tests/runtime_parity/case-mappings.tsv
    sources=lib/tests
    targets=(--lib --tests)
    features=(--locked)
    case "$profile" in
        no-default-features) features+=(--no-default-features) ;;
        extra-capabilities)
            features+=(--no-default-features --features \
                computation-rocksdb-tests,middleware-decoder,middleware-map,middleware-parse-json,middleware-promote,middleware-relabel,middleware-unwind)
            ;;
        integration)
            package=lib-integration-tests
            inventory=lib/tests/runtime_parity/integration-cases.tsv
            mappings=/dev/null
            sources=lib-integration-tests/tests
            targets=(--tests)
            features+=(--no-default-features --features computation-middleware-tests,garnet-tests)
            ;;
        plugin-factories)
            package=drasi-plugin-sdk
            inventory=/dev/null
            mappings=/dev/null
            sources=components/plugin-sdk/tests
            targets=(--test computation_factories)
            features+=(--no-default-features --features computation)
            ;;
    esac
    discovered="$logs/$profile.discovered.tsv"
    : > "$discovered"
    listing="$logs/$profile.list.log"
    if cargo test --color never -p "$package" "${features[@]}" "${targets[@]}" \
        -- --list > "$listing" 2>&1; then
        if ! awk '
            /Running unittests src\/lib.rs / { binary = "lib"; next }
            /Running tests\/[^ ]+\.rs / {
                binary = $2;
                sub(/^tests\//, "", binary);
                sub(/\.rs$/, "", binary);
                next;
            }
            /: test$/ {
                if (binary == "") { invalid = 1; next }
                sub(/: test$/, "");
                print binary "\t" $0;
                count++;
            }
            END { exit invalid || count == 0 }
        ' "$listing" | LC_ALL=C sort > "$discovered"; then
            printf 'Discovery produced no parseable test cases: %s\n' "$listing" >&2
            failed=1
        elif ! awk -F '\t' '
            /^#/ || NF == 0 { next }
            FILENAME == ARGV[1] {
                key = $1 SUBSEP $2;
                if (key in wanted) { print "Duplicate original case: " $1 "::" $2; invalid = 1 }
                wanted[key] = $3;
                original++;
                next;
            }
            FILENAME == ARGV[2] {
                key = $1 SUBSEP $2;
                if (!(key in wanted) || key in mapped || NF < 5 || $5 == "") {
                    print "Invalid case mapping: " $1 "::" $2;
                    invalid = 1;
                }
                mapped[key] = $3 SUBSEP $4;
                next;
            }
            {
                key = $1 SUBSEP $2;
                if (key in found) { print "Duplicate discovered case: " $1 "::" $2; invalid = 1 }
                found[key] = 1;
                discovered++;
            }
            END {
                for (key in wanted) {
                    if (key in found) {
                        if (key in mapped) {
                            print "Mapped original name still executes: " key;
                            invalid = 1;
                        }
                        retained++;
                    } else if (key in mapped && mapped[key] in found) {
                        replaced++;
                    } else {
                        split(key, parts, SUBSEP);
                        print "Original case lacks an executing replacement: " parts[1] "::" parts[2];
                        invalid = 1;
                    }
                }
                printf "Baseline: %d original names retained + %d explicit executing case mappings = %d original cases; %d current cases discovered\n", retained, replaced, original, discovered;
                exit invalid;
            }
        ' "$inventory" "$mappings" "$discovered"; then
            failed=1
        fi
        if ! awk -F '\t' -v package="$package" -v profile="$profile" '
            /^#/ || NF == 0 { next }
            FILENAME == ARGV[1] {
                if ($1 == package && ($4 == "all" || $4 == profile)) {
                    required[$2 SUBSEP $3] = $5;
                }
                next;
            }
            { found[$1 SUBSEP $2] = 1 }
            END {
                for (key in required) {
                    if (!(key in found)) {
                        split(key, parts, SUBSEP);
                        print "Missing computation contract: " parts[1] "::" parts[2] " - " required[key];
                        invalid = 1;
                    }
                }
                exit invalid;
            }
        ' lib/tests/runtime_parity/computation-contracts.tsv "$discovered"; then
            failed=1
        fi
        for source in "$sources"/*.rs; do
            binary="${source##*/}"
            binary="${binary%.rs}"
            case "$binary" in
                computation_query_faults|computation_transaction_transformer|computation_pipeline_parity|computation_middleware_recovery)
                    [[ "$profile" == extra-capabilities ]] || continue
                    ;;
            esac
            if ! awk -F '\t' -v binary="$binary" '$1 == binary { found = 1 } END { exit !found }' \
                "$discovered"; then
                printf 'Required suite has no discovered tests in %s: %s\n' "$profile" "$binary" >&2
                failed=1
            fi
        done
        if [[ "$profile" == no-default-features ]]; then
            if [[ ! -s "$logs/default.discovered.tsv" ]]; then
                printf 'Default discovery failed; cannot compare no-default coverage.\n' >&2
                failed=1
            elif ! diff -u "$logs/default.discovered.tsv" "$discovered"; then
                printf 'Default and no-default builds must expose the same runtime tests.\n' >&2
                failed=1
            fi
        elif [[ "$profile" == extra-capabilities ]]; then
            missing="$logs/$profile.missing-default.tsv"
            if [[ ! -s "$logs/default.discovered.tsv" ]]; then
                printf 'Default discovery failed; cannot compare extra-capability coverage.\n' >&2
                failed=1
            elif ! LC_ALL=C comm -23 "$logs/default.discovered.tsv" "$discovered" > "$missing" \
                || [[ -s "$missing" ]]; then
                printf 'Extra capabilities lost default test coverage: %s\n' "$missing" >&2
                failed=1
            fi
        fi
    else
        printf 'Discovery failed: %s\n' "$listing" >&2
        failed=1
    fi
    printf 'Running %s (full log: %s/%s.log)\n' "$profile" "$logs" "$profile"
    cargo test --color never -p "$package" \
        "${features[@]}" "${targets[@]}" --no-fail-fast -- --format pretty > "$logs/$profile.log" 2>&1
    status=$?
    printf '%s\n' "$status" > "$logs/$profile.exit-code"
    grep -E '^test result:|^error:|^failures:' "$logs/$profile.log" || true
    if [[ "$status" != 0 ]]; then
        failed=1
    fi
    executed="$logs/$profile.executed.tsv"
    awk '
        function finish(outcome) {
            print binary "\t" pending "\t" outcome;
            pending = "";
        }
        function abandon() {
            if (pending != "") finish("unreported");
        }
        { gsub(/\033\[[0-9;]*m/, "") }
        /Running unittests src\/lib.rs / { abandon(); binary = "lib"; next }
        /Running tests\/[^ ]+\.rs / {
            abandon();
            binary = $2;
            sub(/^tests\//, "", binary);
            sub(/\.rs$/, "", binary);
            next;
        }
        /^test [^ ]+( - should panic)? \.\.\. / {
            abandon();
            split($0, parts, " ");
            pending = parts[2];
            sub(/^test [^ ]+( - should panic)? \.\.\. /, "");
        }
        pending != "" && /^(ok|FAILED|ignored)([[:space:],]|[12][0-9][0-9][0-9]-|$)/ {
            outcome = $0;
            sub(/[^a-zA-Z].*$/, "", outcome);
            finish(outcome);
        }
        END { abandon() }
    ' "$logs/$profile.log" > "$executed"
    if ! awk -F '\t' -v package="$package" '
        /^#/ || NF == 0 { next }
        FILENAME == ARGV[1] {
            if ($1 == package) allowed[$2 SUBSEP $3] = $4;
            next;
        }
        FILENAME == ARGV[2] { discovered[$1 SUBSEP $2] = 1; count++; next }
        {
            key = $1 SUBSEP $2;
            if (!(key in discovered) || key in outcomes) {
                print "Unexpected or duplicate executed case: " $1 "::" $2;
                invalid = 1;
            }
            outcome = $3;
            sub(/,$/, "", outcome);
            outcomes[key] = outcome;
        }
        END {
            if (!count) {
                print "No discovered cases to verify";
                invalid = 1;
            }
            for (key in discovered) {
                if (outcomes[key] == "ok") { passed++; continue }
                split(key, parts, SUBSEP);
                if (outcomes[key] == "ignored" && key in allowed &&
                    (allowed[key] == "-" || outcomes[parts[1] SUBSEP allowed[key]] == "ok")) {
                    ignored++;
                    continue;
                }
                print "Required case did not pass: " parts[1] "::" parts[2] " (" outcomes[key] ")";
                invalid = 1;
            }
            printf "Execution verified: %d passed; %d explicitly permitted diagnostics/workers ignored\n", passed, ignored;
            exit invalid;
        }
    ' lib/tests/runtime_parity/allowed-ignored.tsv "$discovered" "$executed"; then
        failed=1
    fi
done

# These are capability configurations of one runtime, not engine comparisons.
# Discovery and execution failures remain failures; every profile is attempted.
exit "$failed"
