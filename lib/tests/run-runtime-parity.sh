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

set -o pipefail

cd "$(dirname "$0")/../.." || exit 1
logs="${1:-target/runtime-parity}"
mkdir -p "$logs" || exit 1
inventory=lib/tests/runtime_parity/original-cases.tsv
failed=0

for profile in component-feature-off component-feature-on computation-feature-on; do
    features=()
    mode=component
    case "$profile" in
        component-feature-on) features=(--features computation) ;;
        computation-feature-on) features=(--features computation); mode=computation ;;
    esac
    discovered="$logs/$profile.discovered.tsv"
    : > "$discovered"
    for binary in lib bootstrap_failure_e2e reaction_recovery_e2e metrics_e2e; do
        if [[ "$binary" == lib ]]; then
            target=(--lib)
        else
            target=(--test "$binary")
        fi
        listing="$logs/$profile.$binary.list.log"
        if ! DRASI_TEST_EXECUTION="$mode" cargo test -p drasi-lib \
            "${features[@]}" "${target[@]}" -- --list > "$listing" 2>&1; then
            printf 'Discovery failed: %s\n' "$listing" >&2
            exit 1
        fi
        awk -v binary="$binary" '/: test$/ { sub(/: test$/, ""); print binary "\t" $0 }' \
            "$listing" >> "$discovered"
    done
    if ! awk -F '\t' '
        FNR == NR {
            if ($0 ~ /^#/ || NF == 0) next;
            key = $1 SUBSEP $2;
            if (key in wanted) { print "Duplicate original case: " $1 "::" $2; invalid = 1 }
            wanted[key] = $3;
            count[$3]++;
            next;
        }
        { found[$1 SUBSEP $2] = 1 }
        END {
            for (key in wanted) {
                if (!(key in found)) {
                    split(key, parts, SUBSEP);
                    print "Original case disappeared: " parts[1] "::" parts[2];
                    invalid = 1;
                }
            }
            for (category in count) print category ": " count[category] " original cases";
            exit invalid;
        }
    ' "$inventory" "$discovered"; then
        exit 1
    fi
    printf 'Running %s (full log: %s/%s.log)\n' "$profile" "$logs" "$profile"
    DRASI_TEST_EXECUTION="$mode" cargo test -p drasi-lib \
        "${features[@]}" --lib --tests --no-fail-fast > "$logs/$profile.log" 2>&1
    status=$?
    printf '%s\n' "$status" > "$logs/$profile.exit-code"
    grep -E '^test result:|^error:|^failures:' "$logs/$profile.log" || true
    if [[ "$status" != 0 ]]; then
        failed=1
    fi
done

# Failures, including known pre-existing regressions, are never excluded or
# rewritten into a successful run. All three profiles still get attempted.
exit "$failed"
